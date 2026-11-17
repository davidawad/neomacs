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
//!   test-only detached section) is counted in [`StopWorld`] as running. The
//!   requester raises [`AsyncSource::StopRequest`] in `ASYNC_ATTENTION`, the
//!   word every safe point tests, and waits until no thread is running. A
//!   thread reaching its next safe point copies its roots into a root image
//!   owned by the World's [`StopWorld`] and parks on its condition variable.
//!
//! The requester then runs the collector with its own roots, the World's
//! (parked registry, shared-root transport table, owner slabs), and every
//! parked image; lowers the request; and wakes everyone. A resumed thread
//! re-installs its heap thread-locals before it touches the heap again, so
//! its mirrors of collector state are current.
//!
//! The stop's mutex and condition variable are the only synchronization
//! this adds; every stop is counted (`safepoint_stops`, `collections`,
//! `stop_wait_ns`, `stop_max_ns` in [`super::ParallelStats`]).

use super::super::Context;
use neovm_value::quit::{ASYNC_ATTENTION, AsyncSource};
use neovm_value::value::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;
use std::time::Instant;

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
    /// Detached threads that run, and so may hold roots no one else knows.
    running: u64,
    /// The root image of every parked detached thread, as raw words: a
    /// `Value` never crosses threads, and the words are read back only while
    /// the thread that published them is parked.
    images: HashMap<ThreadId, Vec<usize>>,
}

/// The counters of [`StopWorld`], copied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct StopCounters {
    pub(crate) safepoint_stops: u64,
    pub(crate) collections: u64,
    pub(crate) stop_wait_ns: u64,
    pub(crate) stop_max_ns: u64,
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
                running: 0,
                images: HashMap::new(),
            }),
            changed: Condvar::new(),
            safepoint_stops: AtomicU64::new(0),
            collections: AtomicU64::new(0),
            stop_wait_ns: AtomicU64::new(0),
            stop_max_ns: AtomicU64::new(0),
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

    /// The counters now.
    pub(crate) fn counters(&self) -> StopCounters {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        StopCounters {
            safepoint_stops: load(&self.safepoint_stops),
            collections: load(&self.collections),
            stop_wait_ns: load(&self.stop_wait_ns),
            stop_max_ns: load(&self.stop_max_ns),
        }
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

    /// The calling thread, running detached, stops running: it publishes
    /// IMAGE and the requester (if any) may stop waiting for it.
    pub(crate) fn park(&self, image: Vec<usize>) {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        debug_assert!(state.running > 0, "only a running thread parks");
        state.running -= 1;
        state.images.insert(me, image);
        drop(state);
        self.changed.notify_all();
    }

    /// The calling thread, parked, runs again once no stop is in progress.
    /// Returns its image's storage for reuse.
    pub(crate) fn resume(&self) -> Vec<usize> {
        let me = std::thread::current().id();
        let mut state = self.lock_state();
        while state.phase != Phase::Running {
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
        drop(state);
        self.changed.notify_all();
    }

    /// The calling thread holds the Baton: stop every detached thread, run
    /// COLLECT with the roots of every parked image, and resume them.
    ///
    /// A collection requested while the World is already stopped (the
    /// collector's own `post-gc-hook` calling `garbage-collect`, or a peer
    /// the hook let run) collects inside that stop.
    pub(crate) fn collect_stopped(&self, collect: impl FnOnce(&[Value])) {
        let requested = Instant::now();
        let mut state = self.lock_state();
        match state.phase {
            Phase::Running => {}
            Phase::Stopped => {
                let roots = image_roots(&state);
                drop(state);
                collect(&roots);
                return;
            }
            // Only the Baton holder requests, and it does not request again
            // while it waits.
            Phase::Stopping => unreachable!("one stop is requested at a time"),
        }
        state.phase = Phase::Stopping;
        ASYNC_ATTENTION.raise(AsyncSource::StopRequest);
        while state.running > 0 {
            state = self.wait(state);
        }
        state.phase = Phase::Stopped;
        let waited = elapsed_ns(requested);
        let stopped = u64::try_from(state.images.len()).unwrap_or(u64::MAX);
        let roots = image_roots(&state);
        drop(state);
        // Resume everyone even if the collector unwinds: a parked thread
        // must never wait for a stop that no one will end.
        let resume = ResumeOnDrop {
            stop: self,
            requested,
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
    pub(crate) fn collect_with_world_stopped(&mut self, collect: impl FnOnce(&mut Self)) {
        let Some(lock) = self.parallel_lock_shared() else {
            collect(self);
            return;
        };
        lock.stop().collect_stopped(|roots| {
            let saved = super::super::save_scratch_gc_roots();
            super::super::push_scratch_gc_roots(roots);
            collect(self);
            super::super::restore_scratch_gc_roots(saved);
        });
    }
}

/// The integration point for a thread whose allocation tripped the
/// collection threshold (S3 calls it once it has attached; this tree calls
/// it from the threshold check every allocation reaches, the GC safe point):
/// the thread becomes the requester of a stop-the-world collection.
pub(crate) fn on_threshold_tripped(ctx: &mut Context) {
    request_collection(ctx);
}

/// Collect now, as the requester: the caller holds the Baton.
pub(crate) fn request_collection(ctx: &mut Context) {
    ctx.gc_collect_from_current_roots();
}

/// `maybe_quit_slow`'s part: a stop is requested. A thread running detached
/// Lisp parks here with its roots; the Baton holder (the requester itself,
/// or a holder that saw a stale hint) has nothing to do.
pub(crate) fn stop_safepoint(ctx: &mut Context) {
    let Some(lock) = ctx.parallel_lock_shared() else {
        return;
    };
    if lock.is_attached_here() {
        return;
    }
    lock.stop().park(owner_image(ctx));
    lock.stop().resume();
    // The collector changed heap state this thread mirrors.
    ctx.tagged_heap.install_tagged_heap();
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
