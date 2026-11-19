//! Stop-the-world collection in a Parallel World (feature `shared-heap`).
//!
//! A collection in a Parallel World runs on the thread holding the Baton,
//! the requester, with the existing synchronous collector (the Baton keeps
//! the concurrent marker excluded, `green_owner` point 5). Before it marks,
//! every other thread must be stopped somewhere its roots are known:
//!
//! - a thread blocked in `thread-join`, `mutex-lock`, `condition-wait`,
//!   `thread-yield` or a wait for the Baton itself parked through Green's
//!   park path, which published its private and thread-local roots in the
//!   World's parked registry before it blocked. It is stopped already and the
//!   collector traces that registry.
//! - a thread running detached (S3's detached execution, and this slice's
//!   test-only detached section) is counted in [`StopWorld`] as running,
//!   from the moment it puts the Baton back to run detached until it holds
//!   the Baton again for World work; a heap access it attaches for keeps it
//!   counted, since its Rust frames hold values no image names. The
//!   requester raises [`AsyncSource::StopRequest`] in `ASYNC_ATTENTION` and
//!   waits until no thread is running. A thread reaching its next exact
//!   safe point (a collection point of the evaluator, the VM or compiled
//!   code, where every value it holds is rooted in its Context) copies its
//!   roots into a root image owned by the World's [`StopWorld`], closes its
//!   allocation runs and parks on its condition variable.
//!
//! The requester waits WITHOUT the Baton: it parks as `sleep-for` does, for
//! [`STOP_POLL`] at a time (Green's park publishes its roots and puts the
//! Baton back), until no thread runs. A running thread blocked attaching
//! (for a `World`-class primitive or a heap access) gets the Baton
//! meanwhile, finishes that work and parks at its next safe point; were the
//! requester to wait holding the Baton, that thread would wait for it
//! forever. Any Baton holder that finds no thread running may collect for
//! the stop, and a requester that finds a collection ran meanwhile does not
//! collect again.
//!
//! The collector then runs with its own roots, the World's (parked registry,
//! shared-root transport table, owner slabs), and every parked image, and
//! finishes the cycle's sweep before the parked threads resume, so they
//! never run in a cycle between its slices. The parked threads sweep with
//! it: while they wait for the stop to end they take the shares of its
//! in-stop sweep ([`StopWorld::run_on_parked`]). It lowers the request and wakes
//! everyone. A resumed thread re-installs its heap thread-locals, briefly
//! attached, before it touches the heap again, so its mirrors of collector
//! state are current.
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
use std::collections::HashMap;
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
    /// No collection: detached threads run.
    Running,
    /// A requester waits for the running threads to park.
    Stopping,
    /// Every detached thread is parked; the requester collects.
    Stopped,
}

/// The stop's state, under its mutex.
struct StopState {
    phase: Phase,
    /// When the stop in progress was requested.
    requested_at: Option<Instant>,
    /// Detached threads that run, and so may hold roots no one else knows.
    running: u64,
    /// The root image of every parked detached thread, as raw words: a
    /// `Value` never crosses threads, and the words are read back only while
    /// the thread that published them is parked.
    images: HashMap<ThreadId, Vec<usize>>,
    /// Work the collecting thread hands its parked threads
    /// ([`StopWorld::run_on_parked`]), while it does.
    shares: Option<ParkedShares>,
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
    /// No thread runs: the World is stopped for the caller to collect.
    Stopped,
    /// The World was stopped already (a collection inside a collection).
    Nested,
    /// Threads still run; the stop is requested, and the caller waits for
    /// them without the Baton, then asks again.
    Busy,
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
    /// Parks of a requester waiting for running threads, each one detach
    /// and one attach of the Baton.
    pub(crate) requester_parks: u64,
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
    requester_parks: AtomicU64,
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
                running: 0,
                images: HashMap::new(),
                shares: None,
            }),
            changed: Condvar::new(),
            safepoint_stops: AtomicU64::new(0),
            collections: AtomicU64::new(0),
            stop_wait_ns: AtomicU64::new(0),
            stop_max_ns: AtomicU64::new(0),
            requester_parks: AtomicU64::new(0),
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
    /// waiters once none runs, under STATE. A waiter on the running count
    /// (a stop's requester, a holder waiting in place) waits for it to
    /// reach zero; a parked thread waits for the stop to end or for shares,
    /// which a park does not change. Woken at every park, each parked
    /// thread woke again for every thread that parked after it.
    fn notify_if_none_runs(&self, state: &StopState) {
        if state.running == 0 {
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
            requester_parks: load(&self.requester_parks),
        }
    }

    /// A requester parked once while threads still ran
    /// ([`Context::collect_with_world_stopped`]).
    fn requester_parked(&self) {
        self.requester_parks.fetch_add(1, Ordering::Relaxed);
    }

    /// The calling thread, holding the Baton, is about to run detached: it
    /// starts parked with IMAGE, so a collection that starts before it runs
    /// already knows its roots. [`Self::resume`] makes it run.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "S3's detached execution is the production caller")
    )]
    pub(crate) fn enter_parked(&self, image: Vec<usize>) {
        let me = std::thread::current().id();
        let previous = self.lock_state().images.insert(me, image);
        debug_assert!(previous.is_none(), "a thread runs one detached extent");
    }

    /// The calling thread, holding the Baton, puts it back to run detached.
    /// It runs even while a stop is requested: it holds the Baton, so the
    /// stop has not begun, and it parks at its next safe point.
    pub(crate) fn enter_running(&self) {
        self.lock_state().running += 1;
    }

    /// Whether a stop is requested or in progress (exact, unlike the
    /// `ASYNC_ATTENTION` hint).
    pub(crate) fn stop_pending(&self) -> bool {
        self.lock_state().phase != Phase::Running
    }

    /// The calling thread, running detached, claimed a due collection: it
    /// requests a stop and parks with IMAGE like any running thread, until
    /// the stop ends (false: it runs again) or no thread runs (true: it
    /// attaches and collects for the stop, then [`Self::withdraw_image`]).
    pub(crate) fn request_and_park(&self, image: Vec<usize>) -> bool {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        debug_assert!(state.running > 0, "only a running thread parks");
        state.running -= 1;
        state.images.insert(me, image);
        if state.phase == Phase::Running {
            state.requested_at = Some(Instant::now());
            state.phase = Phase::Stopping;
            ASYNC_ATTENTION.raise(AsyncSource::StopRequest);
        }
        self.notify_if_none_runs(&state);
        loop {
            state = self.help(state);
            match state.phase {
                Phase::Running => {
                    state.images.remove(&me);
                    state.running += 1;
                    return false;
                }
                Phase::Stopping if state.running == 0 => return true,
                Phase::Stopping | Phase::Stopped => state = self.wait(state),
            }
        }
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

    /// The requester of [`Self::request_and_park`] holds the Baton: its
    /// roots are its Context's again.
    pub(crate) fn withdraw_image(&self) {
        let me = std::thread::current().id();
        self.lock_state().images.remove(&me);
    }

    /// The calling thread, running detached, stops running: it publishes
    /// IMAGE and the requester (if any) may stop waiting for it.
    pub(crate) fn park(&self, image: Vec<usize>) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        debug_assert!(state.running > 0, "only a running thread parks");
        state.running -= 1;
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
        state.running += 1;
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

    /// The calling thread stops running detached for good (it is about to
    /// attach, or to end).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "S3's detached execution is the production caller")
    )]
    pub(crate) fn leave(&self) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        debug_assert!(state.running > 0, "only a running thread leaves");
        state.running -= 1;
        state.images.remove(&me);
        self.notify_if_none_runs(&state);
    }

    /// The calling thread holds the Baton and asks to stop every detached
    /// thread. With none running the World is stopped now; otherwise the
    /// stop is requested (once, by whichever holder asks first) and a
    /// caller that CAN_WAIT waits without the Baton, then asks again.
    pub(crate) fn request(&self, can_wait: bool) -> Requested {
        let mut state = self.lock_state();
        match state.phase {
            Phase::Stopped => Requested::Nested,
            Phase::Running | Phase::Stopping if state.running == 0 => {
                state.requested_at.get_or_insert_with(Instant::now);
                state.phase = Phase::Stopped;
                Requested::Stopped
            }
            _ if !can_wait => Requested::Refused,
            Phase::Running => {
                state.requested_at = Some(Instant::now());
                state.phase = Phase::Stopping;
                ASYNC_ATTENTION.raise(AsyncSource::StopRequest);
                Requested::Busy
            }
            Phase::Stopping => Requested::Busy,
        }
    }

    /// The calling thread holds the Baton: stop every detached thread,
    /// waiting for them in place, run COLLECT with the roots of every parked
    /// image, and resume them.
    ///
    /// A collection requested while the World is already stopped (the
    /// collector's own `post-gc-hook` calling `garbage-collect`, or a peer
    /// the hook let run) collects inside that stop.
    pub(crate) fn collect_stopped(&self, collect: impl FnOnce(&[Value])) {
        let mut requested = self.request(true);
        while requested == Requested::Busy {
            self.wait_running_in_place();
            requested = self.request(true);
        }
        self.collect_requested(requested, collect);
    }

    /// Wait, holding whatever the caller holds, until no thread runs.
    fn wait_running_in_place(&self) {
        let mut state = self.lock_state();
        while state.running > 0 {
            state = self.wait(state);
        }
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
        ASYNC_ATTENTION.take(AsyncSource::StopRequest);
        state.phase = Phase::Running;
        state.requested_at = None;
        drop(state);
        self.collections.fetch_add(1, Ordering::Relaxed);
        self.safepoint_stops.fetch_add(stopped, Ordering::Relaxed);
        self.stop_wait_ns.fetch_add(waited, Ordering::Relaxed);
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
    /// at a time, and does not collect when another holder collected
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
                    // The driver's body has not begun: this park moves the
                    // Baton as `sleep-for` does, which an active driver
                    // refuses.
                    let driver = std::mem::replace(&mut self.gc_driver_active, false);
                    let wait_end = Instant::now() + STOP_POLL;
                    let parked = self.green_park(Wait::Select { wait_end });
                    self.gc_driver_active = driver;
                    match parked {
                        Ok(()) => stop.requester_parked(),
                        // The park refused before the Baton moved: wait for
                        // the stop in place, as a bare requester does.
                        Err(_) => stop.wait_running_in_place(),
                    }
                    if stop.counters().collections != seen {
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

    /// `gc_collect_from_current_roots` on a thread running detached, at an
    /// exact safe point (its caller rooted what it holds): park for the
    /// stop in progress, or request the collection the World has due, park
    /// like the others, and collect once no thread runs.
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

    /// The due collection this detached thread claimed: request the stop and
    /// park; when every other detached thread has parked, attach (no longer
    /// running), collect, and run detached again. A stop another holder ends
    /// first is the collection.
    fn parallel_request_collection(&mut self, lock: &BatonLock, force_complete: bool) {
        let stop = lock.stop();
        let seen = stop.counters().collections;
        let image = owner_image(self);
        let Some(gate) = neovm_value::detached::suspend_detached() else {
            return;
        };
        if stop.request_and_park(image) {
            self.parallel_seat_baton(lock.attach().into_baton());
            stop.withdraw_image();
            if stop.counters().collections == seen {
                self.gc_collect_from_current_roots_impl(force_complete);
            } else {
                self.tagged_heap.install_tagged_heap();
            }
            let baton = self.parallel_unseat_baton();
            lock.body_detached();
            lock.detach(baton);
        } else if stop.counters().collections != seen {
            let baton = lock.attach().into_baton();
            self.tagged_heap.install_tagged_heap();
            lock.detach(baton);
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
        let seen = stop.counters().collections;
        let mut image = owner_image(self);
        image.extend(held.iter().map(|value| value.bits()));
        let Some(gate) = neovm_value::detached::suspend_detached() else {
            return;
        };
        stop.park(image);
        stop.resume();
        if stop.counters().collections != seen {
            // The collection changed heap state this thread mirrors; the
            // mirrors are refreshed under the Baton, as at any install.
            let baton = lock.attach().into_baton();
            self.tagged_heap.install_tagged_heap();
            lock.detach(baton);
        }
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
            stop.enter_parked(self.held.clone());
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
            lock.attach().into_baton()
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
