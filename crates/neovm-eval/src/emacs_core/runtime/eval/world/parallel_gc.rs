//! Stop-the-world collection in a Parallel World (feature `shared-heap`).
//!
//! A collection in a Parallel World runs on the thread that requests it,
//! with the existing synchronous collector. Before it marks, every other
//! mutator must be stopped somewhere its roots are known. [`StopWorld`]
//! keeps the set of mutators that run; a stop is established once that set
//! holds nobody but the requester, as in a JVM or Go safepoint rendezvous:
//!
//! - a thread blocked in a wait (`thread-join`, `mutex-lock`,
//!   `condition-wait`, `thread-yield`'s park, `sleep-for`,
//!   `accept-process-output`, the main owner's Green park) is at a safe
//!   point already. Its park published its private and thread-local roots,
//!   and it leaves the running set as it puts the Baton back
//!   ([`BatonLock::detach_parked`]). When its wait ends while a stop is in
//!   progress it waits for the stop to end before it takes the Baton or
//!   touches the heap ([`BatonLock::attach_unparked`]); the main owner's
//!   10 ms service turn wakes through the same door.
//! - a thread that runs, detached or holding the Baton for World work,
//!   stays in the running set. The requester raises
//!   [`AsyncSource::StopRequest`] in `ASYNC_ATTENTION`; each running thread
//!   reaches its next exact safe point (a collection point of the
//!   evaluator, the VM or compiled code, where every value it holds is
//!   rooted in its Context). A detached one copies its roots into a root
//!   image owned by the World's [`StopWorld`], closes its allocation runs
//!   and parks on the stop's condition variable; one holding the Baton
//!   parks as a wait does.
//!
//! The requester takes no Baton to establish the stop: a detached requester
//! waits on the stop's condition variable until the running set is empty
//! but for itself, so it never queues behind another holder's turn, and no
//! parked owner has to take a turn for it. Only then does it seat the
//! Baton, which is at rest because every stopped mutator put it back: the
//! collector body needs the World's carried state (GC roots among it) that
//! travels with the Baton, and the parked registry it holds.
//!
//! The collector then runs with its own roots, the World's (parked registry,
//! shared-root transport table, owner slabs), and every parked image, and
//! finishes the cycle's sweep before the parked threads resume, so they
//! never run in a cycle between its slices. The image-parked threads sweep
//! with it: while they wait for the stop to end they take the shares of its
//! in-stop sweep ([`StopWorld::run_on_parked`]). It lowers the request and
//! wakes everyone. A resumed detached thread re-installs its heap
//! thread-locals before it touches the heap again, so its mirrors of
//! collector state are current.
//!
//! The stop's mutex and condition variable are the only synchronization
//! this adds; every stop is counted (`safepoint_stops`, `collections`,
//! `stop_wait_ns`, `stop_max_ns` in [`super::ParallelStats`]).

use super::super::Context;
use super::BatonLock;
use crate::emacs_core::eval::green::Wait;
use neovm_value::quit::{ASYNC_ATTENTION, AsyncSource};
use neovm_value::tagged::gc::MarkPhase;
use neovm_value::value::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

#[cfg(test)]
#[path = "tests/parallel_gc_test.rs"]
mod tests;

/// Where the World's stop is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// No collection: mutators run.
    Running,
    /// A requester waits for the running mutators to park.
    Stopping,
    /// Every mutator but the requester is parked; the requester collects.
    Stopped,
    /// The collection is over and the requester waits: the threads parked
    /// for the stop refresh their heap mirrors, one at a time.
    Refreshing,
}

/// The stop's state, under its mutex.
struct StopState {
    phase: Phase,
    /// When the stop in progress was requested.
    requested_at: Option<Instant>,
    /// The mutator whose stop is in progress: the only one that runs in it.
    requester: Option<ThreadId>,
    /// Whether the requester's collector took the stop: a collection it
    /// requests from then on runs inside that one.
    collecting: bool,
    /// The mutators that run, detached or holding the Baton, and so may
    /// hold roots no one else knows. A thread parked in a wait or with an
    /// image is not among them.
    running: HashSet<ThreadId>,
    /// The root image of every parked detached thread, as raw words: a
    /// `Value` never crosses threads, and the words are read back only while
    /// the thread that published them is parked.
    images: HashMap<ThreadId, Vec<usize>>,
    /// How many threads parked for the stop have yet to refresh their heap
    /// mirrors, or to find that no collection ran
    /// ([`StopWorld::park_for_stop`]). The stop does not end before.
    refreshes_due: usize,
    /// Work the collecting thread hands its parked threads
    /// ([`StopWorld::run_on_parked`]), while it does.
    shares: Option<ParkedShares>,
}

impl StopState {
    /// Whether a mutator other than ME runs.
    fn others_run(&self, me: ThreadId) -> bool {
        self.running.iter().any(|thread| *thread != me)
    }

    /// Whether THREAD may run: no stop is in progress, or the stop is its
    /// own.
    fn admits(&self, thread: ThreadId) -> bool {
        self.phase == Phase::Running || self.requester == Some(thread)
    }

    /// Back to no stop.
    fn clear_stop(&mut self) {
        ASYNC_ATTENTION.take(AsyncSource::StopRequest);
        self.phase = Phase::Running;
        self.requested_at = None;
        self.requester = None;
        self.collecting = false;
    }
}

/// The shares of one [`StopWorld::run_on_parked`] call.
struct ParkedShares {
    task: ShareTask,
    parts: usize,
    /// The next share no thread has taken.
    next: usize,
    /// Shares that have returned.
    finished: usize,
    /// The first share's panic, for the caller to resume.
    panic: Option<Box<dyn std::any::Any + Send>>,
}

/// A borrowed `&(dyn Fn(usize) + Sync)` of the [`StopWorld::run_on_parked`]
/// call in progress, as an address: its borrow lives on that caller's stack.
#[derive(Clone, Copy)]
struct ShareTask(*const ());

// SAFETY: the task is `Sync`, and `run_on_parked` does not return, so its
// borrow stays live, until every share that took the address has returned.
unsafe impl Send for ShareTask {}

impl ShareTask {
    /// Run share INDEX.
    ///
    /// # Safety
    /// The `run_on_parked` call that made this task has not returned.
    unsafe fn run(self, index: usize) {
        // SAFETY: the address is that call's `&&(dyn Fn(usize) + Sync)`,
        // live per the caller's contract.
        let task = unsafe { *self.0.cast::<&(dyn Fn(usize) + Sync)>() };
        task(index);
    }
}

/// How long a requester parks between looks at the running count.
const STOP_POLL: Duration = Duration::from_micros(50);

/// What a stop request found ([`StopWorld::request`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Requested {
    /// No other thread runs: the World is stopped for the caller to collect.
    Stopped,
    /// The caller collects already (a collection inside its collection).
    Nested,
    /// Threads still run; the stop is requested, and the caller waits for
    /// them without the Baton, then asks again.
    Busy,
    /// Another thread's stop is in progress: the caller parks for it as
    /// for a wait, and that stop's collection is the collection.
    Join,
    /// Threads still run and the caller cannot wait: nothing was requested.
    Refused,
}

/// The counters of [`StopWorld`], copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StopCounters {
    pub(crate) safepoint_stops: u64,
    pub(crate) collections: u64,
    pub(crate) stop_wait_ns: u64,
    pub(crate) stop_max_ns: u64,
    /// The longest wait from a stop's request to its collector's start.
    pub(crate) stop_wait_max_ns: u64,
    /// Parks of a requester waiting for running threads, each one detach
    /// and one attach of the Baton.
    pub(crate) requester_parks: u64,
    /// Wakes of parked threads that waited for a stop to end before they
    /// took the Baton.
    pub(crate) waker_waits: u64,
}

/// The rendezvous of a Parallel World's collections, kept beside the Baton
/// in its lock.
pub(crate) struct StopWorld {
    state: Mutex<StopState>,
    /// Signalled when a thread parks or leaves, and when a stop ends.
    changed: Condvar,
    safepoint_stops: AtomicU64,
    collections: AtomicU64,
    stop_wait_ns: AtomicU64,
    stop_max_ns: AtomicU64,
    stop_wait_max_ns: AtomicU64,
    requester_parks: AtomicU64,
    waker_waits: AtomicU64,
    /// The notifications [`Self::notify_if_none_runs`] made.
    #[cfg(test)]
    none_running_notifies: AtomicU64,
}

fn elapsed_ns(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl Default for StopWorld {
    fn default() -> Self {
        Self::new()
    }
}

impl StopWorld {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(StopState {
                phase: Phase::Running,
                requested_at: None,
                requester: None,
                collecting: false,
                running: HashSet::new(),
                images: HashMap::new(),
                refreshes_due: 0,
                shares: None,
            }),
            changed: Condvar::new(),
            safepoint_stops: AtomicU64::new(0),
            collections: AtomicU64::new(0),
            stop_wait_ns: AtomicU64::new(0),
            stop_max_ns: AtomicU64::new(0),
            stop_wait_max_ns: AtomicU64::new(0),
            requester_parks: AtomicU64::new(0),
            waker_waits: AtomicU64::new(0),
            #[cfg(test)]
            none_running_notifies: AtomicU64::new(0),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, StopState> {
        // Each critical section moves plain fields and cannot panic half-way.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, StopState>) -> MutexGuard<'a, StopState> {
        self.changed
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// A thread stopped running (it parked, or left for good): wake the
    /// waiters once none runs but a stop's requester, under STATE. A waiter
    /// on the running set (a stop's requester) waits for that; a parked
    /// thread waits for the stop to end or for shares, which a park does
    /// not change. Woken at every park, each parked thread woke again for
    /// every thread that parked after it.
    fn notify_if_none_runs(&self, state: &StopState) {
        let none_runs = match state.requester {
            Some(requester) => !state.others_run(requester),
            None => state.running.is_empty(),
        };
        if none_runs {
            #[cfg(test)]
            self.none_running_notifies.fetch_add(1, Ordering::Relaxed);
            self.changed.notify_all();
        }
    }

    /// The counters now.
    pub(crate) fn counters(&self) -> StopCounters {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        StopCounters {
            safepoint_stops: load(&self.safepoint_stops),
            collections: load(&self.collections),
            stop_wait_ns: load(&self.stop_wait_ns),
            stop_max_ns: load(&self.stop_max_ns),
            stop_wait_max_ns: load(&self.stop_wait_max_ns),
            requester_parks: load(&self.requester_parks),
            waker_waits: load(&self.waker_waits),
        }
    }

    /// A requester parked once while threads still ran
    /// ([`Context::collect_with_world_stopped`]).
    fn requester_parked(&self) {
        self.requester_parks.fetch_add(1, Ordering::Relaxed);
    }

    /// The calling thread is about to run detached in a test: it starts
    /// parked with IMAGE, so a collection that starts before it runs
    /// already knows its roots. [`Self::resume`] makes it run.
    #[cfg(test)]
    pub(crate) fn enter_parked(&self, image: Vec<usize>) {
        let me = std::thread::current().id();
        let previous = self.lock_state().images.insert(me, image);
        debug_assert!(previous.is_none(), "a thread runs one detached extent");
    }

    /// Whether a stop is requested or in progress (exact, unlike the
    /// `ASYNC_ATTENTION` hint).
    pub(crate) fn stop_pending(&self) -> bool {
        self.lock_state().phase != Phase::Running
    }

    /// The calling thread parks in a wait, where it is at a safe point: its
    /// park published its roots, and it touches no heap state until
    /// [`Self::try_enter_running`] admits it again. A stop's requester may
    /// stop waiting for it.
    pub(crate) fn leave_running(&self) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        state.running.remove(&me);
        // A requester parking between its own looks wakes no one for it.
        if state.requester != Some(me) {
            self.notify_if_none_runs(&state);
        }
    }

    /// The calling thread's wait ended. While another thread's stop is in
    /// progress it blocks, so it neither takes the Baton from the collector
    /// nor touches the heap under it, and it runs again from the moment the
    /// stop ends (true): a later stop waits for it as for any running
    /// thread. Admitted only once it held the Baton, it would lose to the
    /// next stop again and again while collections follow each other
    /// closely. With no such stop (the stop's own requester passes too) it
    /// returns at once (false), and the caller enters with
    /// [`Self::try_enter_running`] once it holds the Baton.
    #[must_use = "a thread that did not wait is not running yet"]
    pub(crate) fn enter_running_after_stop(&self) -> bool {
        let me = std::thread::current().id();
        let state = self.lock_state();
        if state.admits(me) {
            return false;
        }
        self.enter_running_locked(state, me);
        true
    }

    /// [`Self::enter_running_after_stop`] for a thread that
    /// [`Self::try_enter_running`] refused: it runs again when the stop
    /// ends, or at once when it already did.
    pub(crate) fn enter_running_when_admitted(&self) {
        let me = std::thread::current().id();
        self.enter_running_locked(self.lock_state(), me);
    }

    fn enter_running_locked(&self, mut state: MutexGuard<'_, StopState>, me: ThreadId) {
        self.waker_waits.fetch_add(1, Ordering::Relaxed);
        while !state.admits(me) {
            state = self.wait(state);
        }
        state.running.insert(me);
    }

    /// The calling thread, holding the Baton after a wait, runs again:
    /// false when another thread's stop began while it waited for the
    /// Baton, and the caller must put the Baton back for the collector
    /// ([`Self::enter_running_when_admitted`]).
    #[must_use = "a refused thread must not run"]
    pub(crate) fn try_enter_running(&self) -> bool {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        let admitted = state.admits(me);
        if admitted {
            state.running.insert(me);
        }
        admitted
    }

    /// The calling thread, running detached at an exact safe point, claimed
    /// a due collection. It requests the stop and waits here, with no
    /// Baton, until every other mutator is parked (true: the World is
    /// stopped for it to collect). When another thread's stop is already in
    /// progress it parks for that one with IMAGE and REFRESH instead, as
    /// [`Self::park_for_stop`] does (false: it runs again).
    pub(crate) fn stop_from_detached(&self, image: Vec<usize>, refresh: impl FnOnce()) -> bool {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        if state.phase != Phase::Running {
            self.park_locked(state, image, refresh);
            return false;
        }
        debug_assert!(state.running.contains(&me), "a detached requester runs");
        state.requested_at = Some(Instant::now());
        state.requester = Some(me);
        state.phase = Phase::Stopping;
        ASYNC_ATTENTION.raise(AsyncSource::StopRequest);
        while state.others_run(me) {
            state = self.wait(state);
        }
        state.phase = Phase::Stopped;
        true
    }

    /// End a stop of the calling thread's that no collection took: its
    /// requester could not wait for the running threads, or its collector
    /// declined to run. The parked threads resume.
    pub(crate) fn abandon(&self) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        if state.phase == Phase::Running || state.requester != Some(me) || state.collecting {
            return;
        }
        state.clear_stop();
        drop(state);
        self.changed.notify_all();
    }

    /// How many threads are parked with an image.
    fn parked(&self) -> usize {
        self.lock_state().images.len()
    }

    /// The calling thread collects for the stop: run TASK for each share
    /// index in `0..parts`, here and on the parked threads, which take the
    /// shares one at a time while they wait for the stop to end. Returns
    /// once every share has returned, resuming the first share's panic.
    fn run_on_parked(&self, parts: usize, task: &(dyn Fn(usize) + Sync)) {
        let mut state = self.lock_state();
        debug_assert!(state.shares.is_none(), "one share run at a time");
        state.shares = Some(ParkedShares {
            task: ShareTask(std::ptr::from_ref(&task).cast()),
            parts,
            next: 0,
            finished: 0,
            panic: None,
        });
        self.changed.notify_all();
        state = self.help(state);
        // The task's borrow must outlive every share a parked thread took.
        while state
            .shares
            .as_ref()
            .is_some_and(|shares| shares.finished < shares.parts)
        {
            state = self.wait(state);
        }
        let shares = state
            .shares
            .take()
            .expect("the run's shares stay until here");
        drop(state);
        if let Some(panic) = shares.panic {
            std::panic::resume_unwind(panic);
        }
    }

    /// Run shares of a [`Self::run_on_parked`] in progress until none is
    /// left to take.
    fn help<'a>(&'a self, mut state: MutexGuard<'a, StopState>) -> MutexGuard<'a, StopState> {
        while let Some(shares) = state
            .shares
            .as_mut()
            .filter(|shares| shares.next < shares.parts)
        {
            let index = shares.next;
            shares.next += 1;
            let task = shares.task;
            drop(state);
            // SAFETY: `run_on_parked` waits for this share's `finished`
            // count below before it returns.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                task.run(index)
            }));
            state = self.lock_state();
            let shares = state
                .shares
                .as_mut()
                .expect("shares stay until all finished");
            shares.finished += 1;
            if let Err(panic) = outcome {
                shares.panic.get_or_insert(panic);
            }
            if shares.finished == shares.parts {
                self.changed.notify_all();
            }
        }
        state
    }

    /// The calling thread, running detached, parks for the stop in progress
    /// with IMAGE and runs again once it ends. When a collection ran,
    /// REFRESH runs first, inside the stop: it brings this thread's mirrors
    /// of the heap up to date, which reads and writes state the World
    /// shares unsynchronized (the heap's barrier windows, its collection-read
    /// owner and that owner's `Rc` count). Inside the stop the parked
    /// threads run it one at a time, under the stop's mutex, while the
    /// collector waits and no thread holds the Baton to run: no Baton turn
    /// per parked thread is needed for it.
    pub(crate) fn park_for_stop(&self, image: Vec<usize>, refresh: impl FnOnce()) {
        self.park_locked(self.lock_state(), image, refresh);
    }

    /// [`Self::park_for_stop`], under STATE.
    fn park_locked<'a>(
        &'a self,
        mut state: MutexGuard<'a, StopState>,
        image: Vec<usize>,
        refresh: impl FnOnce(),
    ) {
        let me = std::thread::current().id();
        let ran = state.running.remove(&me);
        debug_assert!(ran, "only a running thread parks");
        state.images.insert(me, image);
        state.refreshes_due += 1;
        self.notify_if_none_runs(&state);
        let mut refresh = Some(refresh);
        loop {
            state = self.help(state);
            if state.phase == Phase::Refreshing
                && let Some(refresh) = refresh.take()
            {
                // The collector waits for the count below even if the
                // refresh unwinds: a stop must always end.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(refresh));
                state.refreshes_due -= 1;
                if state.refreshes_due == 0 {
                    self.changed.notify_all();
                }
                if let Err(panic) = outcome {
                    drop(state);
                    std::panic::resume_unwind(panic);
                }
            }
            if state.phase == Phase::Running {
                break;
            }
            state = self.wait(state);
        }
        if refresh.is_some() {
            // No collection ran: the stop was withdrawn.
            state.refreshes_due -= 1;
        }
        state.running.insert(me);
        state.images.remove(&me);
    }

    /// The calling thread, running detached, stops running: it publishes
    /// IMAGE and the requester (if any) may stop waiting for it.
    pub(crate) fn park(&self, image: Vec<usize>) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        let ran = state.running.remove(&me);
        debug_assert!(ran, "only a running thread parks");
        state.images.insert(me, image);
        self.notify_if_none_runs(&state);
    }

    /// The calling thread, parked, runs again once no stop is in progress.
    /// Returns its image's storage for reuse.
    pub(crate) fn resume(&self) -> Vec<usize> {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        loop {
            state = self.help(state);
            if state.phase == Phase::Running {
                break;
            }
            state = self.wait(state);
        }
        state.running.insert(me);
        state.images.remove(&me).unwrap_or_default()
    }

    /// The calling thread, running detached, reached a safe point: when a
    /// stop is requested it parks with the image IMAGE builds and resumes
    /// after the collection.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "S3's detached execution is the production caller")
    )]
    pub(crate) fn safepoint(&self, image: impl FnOnce() -> Vec<usize>) {
        if !ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest) {
            return;
        }
        if self.lock_state().phase == Phase::Running {
            // A stale hint: the stop ended between the load and the lock.
            return;
        }
        self.park(image());
        self.resume();
    }

    /// The calling thread stops running detached for good in a test (its
    /// section is over, or it ends).
    #[cfg(test)]
    pub(crate) fn leave(&self) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        let ran = state.running.remove(&me);
        debug_assert!(ran, "only a running thread leaves");
        state.images.remove(&me);
        self.notify_if_none_runs(&state);
    }

    /// The calling thread asks to stop every other mutator, for a
    /// collection it runs itself. With none running the World is stopped
    /// now; otherwise the stop is requested and a caller that CAN_WAIT
    /// parks without the Baton, then asks again. A thread whose stop is
    /// established already ([`Self::stop_from_detached`]) takes it here.
    pub(crate) fn request(&self, can_wait: bool) -> Requested {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        if state.phase != Phase::Running && state.requester != Some(me) {
            return if can_wait {
                Requested::Join
            } else {
                Requested::Refused
            };
        }
        match state.phase {
            Phase::Stopped if state.collecting => Requested::Nested,
            Phase::Stopped => {
                state.collecting = true;
                Requested::Stopped
            }
            Phase::Running | Phase::Stopping if !state.others_run(me) => {
                state.requested_at.get_or_insert_with(Instant::now);
                state.requester = Some(me);
                state.phase = Phase::Stopped;
                state.collecting = true;
                Requested::Stopped
            }
            Phase::Stopping => Requested::Busy,
            // Its requester waits in `end_stop` and asks nothing.
            Phase::Refreshing => Requested::Refused,
            Phase::Running if !can_wait => Requested::Refused,
            Phase::Running => {
                state.requested_at = Some(Instant::now());
                state.requester = Some(me);
                state.phase = Phase::Stopping;
                ASYNC_ATTENTION.raise(AsyncSource::StopRequest);
                Requested::Busy
            }
        }
    }

    /// Stop every running thread, waiting for them in place, run COLLECT
    /// with the roots of every parked image, and resume them: the bare
    /// rendezvous, for its tests.
    #[cfg(test)]
    pub(crate) fn collect_stopped(&self, collect: impl FnOnce(&[Value])) {
        let me = std::thread::current().id();
        let mut requested = self.request(true);
        while requested == Requested::Busy {
            let mut state = self.lock_state();
            while state.others_run(me) {
                state = self.wait(state);
            }
            drop(state);
            requested = self.request(true);
        }
        self.collect_requested(requested, collect);
    }

    /// Run COLLECT for REQUESTED, which [`Self::request`] returned
    /// ([`Requested::Stopped`] or [`Requested::Nested`]), with the roots of
    /// every parked image, and end a stop this request began.
    pub(crate) fn collect_requested(&self, requested: Requested, collect: impl FnOnce(&[Value])) {
        let state = self.lock_state();
        let roots = image_roots(&state);
        if requested == Requested::Nested {
            drop(state);
            collect(&roots);
            return;
        }
        debug_assert_eq!(requested, Requested::Stopped);
        let since = state.requested_at.unwrap_or_else(Instant::now);
        let waited = elapsed_ns(since);
        let stopped = u64::try_from(state.images.len()).unwrap_or(u64::MAX);
        drop(state);
        // Resume everyone even if the collector unwinds: a parked thread
        // must never wait for a stop that no one will end.
        let resume = ResumeOnDrop {
            stop: self,
            requested: since,
            waited,
            stopped,
        };
        collect(&roots);
        drop(resume);
    }

    fn end_stop(&self, requested: Instant, waited: u64, stopped: u64) {
        let mut state = self.lock_state();
        if state.refreshes_due > 0 {
            // Every thread parked for the stop refreshes its heap mirrors
            // before any mutator runs (`Self::park_for_stop`). No thread
            // joins them now: none runs to park.
            state.phase = Phase::Refreshing;
            self.changed.notify_all();
            while state.refreshes_due > 0 {
                state = self.wait(state);
            }
        }
        state.clear_stop();
        drop(state);
        self.collections.fetch_add(1, Ordering::Relaxed);
        self.safepoint_stops.fetch_add(stopped, Ordering::Relaxed);
        self.stop_wait_ns.fetch_add(waited, Ordering::Relaxed);
        self.stop_wait_max_ns.fetch_max(waited, Ordering::Relaxed);
        self.stop_max_ns
            .fetch_max(elapsed_ns(requested), Ordering::Relaxed);
        self.changed.notify_all();
    }
}

/// Every parked image as Values for the collector.
fn image_roots(state: &StopState) -> Vec<Value> {
    state
        .images
        .values()
        .flatten()
        // SAFETY: each word is a Value of this World's heap that its parked
        // thread published while it was rooted there. That thread cannot run
        // until this stop ends, and every collection since it published
        // treated the word as a root, so it still names a live object.
        .map(|&bits| unsafe { Value::from_bits(bits) })
        .collect()
}

struct ResumeOnDrop<'a> {
    stop: &'a StopWorld,
    requested: Instant,
    waited: u64,
    stopped: u64,
}

impl Drop for ResumeOnDrop<'_> {
    fn drop(&mut self) {
        self.stop
            .end_stop(self.requested, self.waited, self.stopped);
    }
}

/// Stops detached threads joined at a primitive's stop point, for tests.
#[cfg(test)]
pub(crate) static JOINED_IN_PRIMITIVES: AtomicU64 = AtomicU64::new(0);

/// The roots only CTX's own thread knows: its private execution state and
/// its thread-local roots (Green's parked census, without the World's).
fn owner_image(ctx: &Context) -> Vec<usize> {
    let mut roots = Vec::new();
    ctx.trace_private_roots(&mut roots);
    let heap = ctx.tagged_heap.tagged_heap_identity();
    let epoch = ctx.tagged_heap.gc_collections();
    let scan = neovm_value::tagged::gc::CacheRootScan::Parked;
    super::super::for_each_thread_local_root_source(heap, epoch, scan, |_, collect| {
        collect(&mut roots);
    });
    roots.into_iter().map(Value::bits).collect()
}

impl Context {
    /// Run the collector body on this thread, the Baton holder, with every
    /// other thread of a Parallel World stopped. Outside one, run it as is.
    ///
    /// While threads still run this thread waits for them parked, [`STOP_POLL`]
    /// at a time. When another thread's stop is in progress it parks for
    /// that one as a wait does, and does not collect when a collection ran
    /// meanwhile. A holder that cannot park (it is not seated with the
    /// Baton) leaves the collection to a detached thread while threads run.
    /// With threads parked, the cycle's sweep finishes inside the stop.
    pub(crate) fn collect_with_world_stopped(&mut self, collect: impl FnOnce(&mut Self)) {
        let Some(lock) = self.parallel_lock_shared() else {
            collect(self);
            return;
        };
        let stop = lock.stop();
        let seen = stop.counters().collections;
        let can_wait = self.green_seat().is_some_and(|seat| seat.holds_baton());
        let requested = loop {
            match stop.request(can_wait) {
                Requested::Refused => {
                    lock.defer_collection(self.tagged_heap.bytes_since_gc() as u64);
                    return;
                }
                Requested::Busy => {
                    if self.park_outside_gc_driver(STOP_POLL).is_err() {
                        // The park refused before the Baton moved, and the
                        // running threads may need it to reach their safe
                        // points: withdraw the request and leave the
                        // collection due.
                        stop.abandon();
                        lock.defer_collection(self.tagged_heap.bytes_since_gc() as u64);
                        return;
                    }
                    stop.requester_parked();
                }
                Requested::Join => {
                    // This park's wake waits for the stop to end
                    // (`BatonLock::attach_unparked`). A refused park keeps
                    // this thread running to its next safe point, where it
                    // asks again.
                    if self.park_outside_gc_driver(Duration::ZERO).is_err()
                        || stop.counters().collections != seen
                    {
                        return;
                    }
                }
                requested => break requested,
            }
        };
        // A collection inside a collection leaves the cycle to the outer one.
        let finish_cycle = requested == Requested::Stopped;
        stop.collect_requested(requested, |roots| {
            let saved = super::super::save_scratch_gc_roots();
            super::super::push_scratch_gc_roots(roots);
            collect(self);
            lock.collection_runs();
            if finish_cycle && !roots.is_empty() {
                // The parked threads' CPUs sweep what owner threads
                // allocate the most of; the slices below finish the rest.
                let workers = std::num::NonZeroUsize::MIN.saturating_add(stop.parked());
                self.tagged_heap
                    .sweep_in_stop_on(workers, |parts, share| stop.run_on_parked(parts, share));
                while matches!(
                    self.tagged_heap.mark_phase(),
                    MarkPhase::Sweeping | MarkPhase::StoppedMarking
                ) {
                    self.gc_collect_from_current_roots_body(false);
                }
            }
            super::super::restore_scratch_gc_roots(saved);
        });
    }

    /// Park this Baton holder for TIMEOUT at a collection's entry, where it
    /// is at a safe point. The driver's body has not begun: the park moves
    /// the Baton as `sleep-for` does, which an active driver refuses.
    fn park_outside_gc_driver(&mut self, timeout: Duration) -> Result<(), super::super::Flow> {
        let driver = std::mem::replace(&mut self.gc_driver_active, false);
        let wait_end = Instant::now() + timeout;
        let parked = self.green_park(Wait::Select { wait_end });
        self.gc_driver_active = driver;
        parked
    }

    /// `gc_collect_from_current_roots` on a thread running detached, at an
    /// exact safe point (its caller rooted what it holds): park for the
    /// stop in progress, or request the collection the World has due and
    /// collect once no other thread runs.
    pub(crate) fn parallel_detached_collect(&mut self, force_complete: bool) {
        let Some(lock) = self.parallel_lock_shared() else {
            return;
        };
        if lock.stop().stop_pending() {
            self.parallel_park_for_stop(&lock, &[]);
        } else if lock.take_collection_due() {
            self.parallel_request_collection(&lock, force_complete);
        }
    }

    /// The due collection this detached thread claimed. It requests the
    /// stop and waits, with no Baton, until every other mutator is parked:
    /// a thread blocked in a wait counts as parked already, so no parked
    /// owner takes a turn for this, and the wait is not queued behind any
    /// holder's turn. Then it seats the Baton, which every stopped mutator
    /// put back, for the collector body: the World's carried state and
    /// parked registry travel with the Baton and hold roots the collector
    /// traces. It collects, and runs detached again. A stop another thread
    /// began first is the collection.
    fn parallel_request_collection(&mut self, lock: &BatonLock, force_complete: bool) {
        let stop = lock.stop();
        let image = owner_image(self);
        // Closes this thread's allocation runs: the collector finds every
        // run closed, its own among them.
        let Some(gate) = neovm_value::detached::suspend_detached() else {
            lock.release_collection_claim();
            return;
        };
        let seen = stop.counters().collections;
        if stop.stop_from_detached(image, || self.tagged_heap.install_tagged_heap()) {
            // Only a thread between its wait's end and its look at the
            // stop can hold the Baton now, and it puts it straight back.
            self.parallel_seat_baton(lock.attach().into_baton());
            self.gc_collect_from_current_roots_impl(force_complete);
            // A collector that returned without taking the stop must not
            // leave the World stopped.
            stop.abandon();
            lock.detach(self.parallel_unseat_baton());
        }
        if stop.counters().collections == seen {
            // No collection ran under the claim: the stop this thread
            // parked for was withdrawn, or the collector refused.
            lock.release_collection_claim();
        }
        neovm_value::detached::resume_detached(gate);
    }

    /// A stop point inside a primitive (`TextHost::join_world_stop`): a
    /// detached thread parks for a requested stop with HELD, the values the
    /// primitive holds, rooted beside its own. Without a request, or
    /// attached, it returns at once.
    pub(crate) fn parallel_join_stop_holding(&mut self, held: &[Value]) {
        if !ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest)
            || !neovm_value::detached::is_detached()
        {
            return;
        }
        let Some(lock) = self.parallel_lock_shared() else {
            return;
        };
        if lock.stop().stop_pending() {
            #[cfg(test)]
            JOINED_IN_PRIMITIVES.fetch_add(1, Ordering::Relaxed);
            self.parallel_park_for_stop(&lock, held);
        }
    }

    /// Park this detached thread for the stop in progress: publish its roots
    /// and HELD, close its allocation runs, and resume once the stop ends.
    fn parallel_park_for_stop(&mut self, lock: &BatonLock, held: &[Value]) {
        let stop = lock.stop();
        let mut image = owner_image(self);
        image.extend(held.iter().map(|value| value.bits()));
        let Some(gate) = neovm_value::detached::suspend_detached() else {
            return;
        };
        // The collection changed heap state this thread mirrors. The
        // mirrors are refreshed inside the stop: under the Baton instead,
        // every awakened worker queued behind the next holder.
        stop.park_for_stop(image, || self.tagged_heap.install_tagged_heap());
        neovm_value::detached::resume_detached(gate);
    }
}

/// The integration point for a thread whose allocation tripped the
/// collection threshold (S3 calls it once it has attached; this tree calls
/// it from the threshold check every allocation reaches, the GC safe point):
/// the thread becomes the requester of a stop-the-world collection.
pub(crate) fn on_threshold_tripped(ctx: &mut Context) {
    request_collection(ctx);
}

/// Collect now, as the requester: the caller holds the Baton, or runs
/// detached and requests the collection attached.
pub(crate) fn request_collection(ctx: &mut Context) {
    ctx.gc_collect_from_current_roots();
}

#[cfg(test)]
pub(crate) mod detached {
    //! The test-only detached section: a Lisp thread of a Parallel World
    //! puts the Baton back and runs a Rust closure over values it holds,
    //! polling the stop at its safe points, until the closure returns.
    //! Slice S3 owns the real detached execution; this is the collector's
    //! test of it.

    use super::super::BatonLock;
    use super::{Context, StopWorld};
    use neovm_value::value::Value;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::ThreadId;

    /// What a section that panicked returns.
    const SECTION_PANICKED: i64 = i64::MIN;

    type SectionBody = Box<dyn FnOnce(&mut Section<'_>) -> i64 + Send>;
    type ThreadBody = Box<dyn FnOnce(&mut Context) -> i64 + Send>;

    /// The detached section each Lisp OS thread is about to run.
    static PENDING: Mutex<Vec<(ThreadId, PendingSection)>> = Mutex::new(Vec::new());
    /// Rust bodies for Lisp threads, by the symbol `make-thread` received.
    static BODIES: Mutex<Vec<(String, ThreadBody)>> = Mutex::new(Vec::new());

    pub(crate) struct PendingSection {
        held: Vec<usize>,
        body: SectionBody,
        out: Arc<AtomicI64>,
    }

    /// What a detached section's closure sees.
    pub(crate) struct Section<'a> {
        stop: &'a StopWorld,
        held: Vec<usize>,
    }

    impl Section<'_> {
        /// The INDEXth value the section was given.
        pub(crate) fn held(&self, index: usize) -> Value {
            // SAFETY: the word is a Value this thread gave the section; the
            // section's image roots it in every collection while it runs.
            unsafe { Value::from_bits(self.held[index]) }
        }

        /// How many collections the World has finished.
        pub(crate) fn collections(&self) -> u64 {
            self.stop.counters().collections
        }

        /// A safe point: park here when a stop is requested.
        pub(crate) fn safepoint(&mut self) {
            let held = &self.held;
            self.stop.safepoint(|| held.clone());
        }
    }

    /// Run BODY detached on CTX's thread over HELD, and return its result.
    pub(crate) fn with_detached(
        ctx: &mut Context,
        held: &[Value],
        body: impl FnOnce(&mut Section<'_>) -> i64 + Send + 'static,
    ) -> i64 {
        let out = Arc::new(AtomicI64::new(0));
        let me = std::thread::current().id();
        PENDING
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                me,
                PendingSection {
                    held: held.iter().map(|value| value.bits()).collect(),
                    body: Box::new(body),
                    out: Arc::clone(&out),
                },
            ));
        // A runnable park whose transfer finds the pending section runs it
        // between its detach and its attach.
        ctx.green_park_runnable_for_test()
            .expect("a Parallel thread parks runnable");
        assert!(
            take_pending_section().is_none(),
            "the park ran the detached section"
        );
        let result = out.load(Ordering::Relaxed);
        assert_ne!(result, SECTION_PANICKED, "the detached section panicked");
        result
    }

    /// The section this OS thread asked to run, if any.
    pub(crate) fn take_pending_section() -> Option<PendingSection> {
        let me = std::thread::current().id();
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        let index = pending.iter().position(|(thread, _)| *thread == me)?;
        Some(pending.swap_remove(index).1)
    }

    impl PendingSection {
        /// Called with the Baton held, from the runnable park's transfer:
        /// put it back, run the section detached, and take it again.
        pub(crate) fn run(
            self,
            lock: &BatonLock,
            baton: super::super::Baton,
        ) -> super::super::Baton {
            let stop = lock.stop();
            // The Baton holder ran: it parks with its image before the
            // Baton goes back, so a collection that starts before the
            // section runs already knows its roots.
            stop.park(self.held.clone());
            lock.detach(baton);
            let mut section = Section {
                stop,
                held: stop.resume(),
            };
            // A failed assertion inside the body must still leave and
            // attach, or every other thread would wait for this one.
            let body = self.body;
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&mut section)));
            stop.leave();
            self.out
                .store(result.unwrap_or(SECTION_PANICKED), Ordering::Relaxed);
            lock.attach_unparked()
        }
    }

    /// Make a Lisp thread started on the symbol NAME run BODY instead.
    pub(crate) fn register_thread_body(
        name: &str,
        body: impl FnOnce(&mut Context) -> i64 + Send + 'static,
    ) {
        BODIES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((name.to_owned(), Box::new(body)));
    }

    /// Whether a thread started on FUNCTION runs a registered body.
    pub(crate) fn has_thread_body(ctx: &Context, function: Value) -> bool {
        let Some(symbol) = function.as_symbol_id(ctx.symbol_mode()) else {
            return false;
        };
        let name = neovm_value::intern::resolve_sym(symbol);
        BODIES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|(known, _)| known.as_str() == name)
    }

    /// The registered body of a thread started on FUNCTION.
    pub(crate) fn take_thread_body(ctx: &Context, function: Value) -> Option<ThreadBody> {
        let symbol = function.as_symbol_id(ctx.symbol_mode())?;
        let name = neovm_value::intern::resolve_sym(symbol);
        let mut bodies = BODIES.lock().unwrap_or_else(PoisonError::into_inner);
        let index = bodies
            .iter()
            .position(|(known, _)| known.as_str() == name)?;
        Some(bodies.swap_remove(index).1)
    }
}
