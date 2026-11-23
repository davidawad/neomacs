//! Stop-the-world collection across a Parallel World's threads: the
//! rendezvous itself, then Lisp threads that run detached sections (the
//! test-only stand-in for S3's detached execution) while other threads
//! collect.

use super::detached::{register_thread_body, with_detached};
use super::{Requested, StopCounters, StopWorld};
use crate::emacs_core::eval::{Context, ThreadingMode};
use crate::emacs_core::print::print_value;
use neovm_value::quit::{ASYNC_ATTENTION, AsyncSource};
use neovm_value::value::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn eval(ev: &mut Context, src: &str) -> String {
    match ev.eval_str(src) {
        Ok(value) => print_value(&value),
        Err(neovm_value::error::EvalError::Signal { symbol, data, .. }) => format!(
            "ERR {} {}",
            neovm_value::intern::resolve_sym(symbol),
            print_value(&Value::list(data))
        ),
        Err(error) => format!("ERR {error:?}"),
    }
}

/// A Parallel World with `pgc-seq` (BASE+1 .. BASE+N) and `pgc-pair` (a
/// list and a copy of it made at once).
fn parallel() -> Context {
    crate::test_utils::init_test_tracing();
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::force_jit_off_for_test(true);
    let mut ev =
        Context::new_with_threading(ThreadingMode::Parallel, crate::test_utils::unit_runtime());
    eval(
        &mut ev,
        "(progn
           (fset 'pgc-seq
                 (lambda (n base)
                   (let ((i base))
                     (mapcar (lambda (_) (setq i (1+ i))) (make-list n nil)))))
           (fset 'pgc-pair
                 (lambda (n base)
                   (let ((l (pgc-seq n base))) (cons l (copy-sequence l))))))",
    );
    ev
}

fn stats(ev: &Context) -> super::super::ParallelStats {
    ev.parallel_lock()
        .expect("the World started its Parallel runtime")
        .stats()
}

/// Sum of BASE+1 .. BASE+N.
fn seq_sum(n: i64, base: i64) -> i64 {
    n * base + n * (n + 1) / 2
}

/// Sum the fixnum list LIST, polling the stop every 64 cells.
fn sum_detached(section: &mut super::detached::Section<'_>, index: usize) -> i64 {
    let mut cell = section.held(index);
    let mut sum = 0;
    let mut seen = 0usize;
    while cell.is_cons() {
        sum += cell
            .cons_car_unobserved()
            .as_fixnum()
            .unwrap_or(i64::MIN / 4);
        cell = cell.cons_cdr_unobserved();
        seen += 1;
        if seen.is_multiple_of(64) {
            section.safepoint();
        }
    }
    sum
}

fn apply(owner: &mut Context, function: &'static str, args: Vec<Value>) -> Value {
    owner
        .apply(Value::symbol(function), args)
        .expect("the test's Lisp helper returns")
}

/// A thread body that builds a list of N, enters a detached section, says
/// it is inside, polls its safe points until RELEASE, and returns its sum.
fn spinner(
    n: i64,
    inside: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
) -> impl FnOnce(&mut Context) -> i64 + Send + 'static {
    move |owner| {
        let list = apply(owner, "pgc-seq", vec![Value::fixnum(n), Value::fixnum(0)]);
        with_detached(owner, &[list], move |section| {
            inside.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) {
                section.safepoint();
                std::thread::yield_now();
            }
            sum_detached(section, 0)
        })
    }
}

/// Let other threads run until FLAG is set.
fn yield_until(ev: &mut Context, flag: &AtomicBool) {
    while !flag.load(Ordering::Acquire) {
        assert_eq!(eval(ev, "(thread-yield)"), "nil");
        std::thread::yield_now();
    }
}

#[test]
fn a_stop_waits_for_every_running_thread_and_roots_its_image() {
    let stop = Arc::new(StopWorld::new());
    let ready = Arc::new(Barrier::new(4));
    let release = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..3i64)
        .map(|k| {
            let (stop, ready, release) =
                (Arc::clone(&stop), Arc::clone(&ready), Arc::clone(&release));
            std::thread::spawn(move || {
                stop.enter_parked(vec![Value::fixnum(100 + k).bits()]);
                stop.resume();
                ready.wait();
                let mut polls = 0u64;
                while !release.load(Ordering::Acquire) {
                    stop.safepoint(|| vec![Value::fixnum(100 + k).bits()]);
                    polls += 1;
                }
                stop.leave();
                polls
            })
        })
        .collect();
    ready.wait();
    let mut seen = Vec::new();
    stop.collect_stopped(|roots| {
        assert!(ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest));
        seen = roots.iter().filter_map(|root| root.as_fixnum()).collect();
    });
    assert!(!ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest));
    seen.sort_unstable();
    assert_eq!(seen, vec![100, 101, 102]);
    release.store(true, Ordering::Release);
    for worker in workers {
        worker.join().expect("worker left the stop");
    }
    assert_eq!(
        stop.counters(),
        StopCounters {
            safepoint_stops: 3,
            collections: 1,
            ..stop.counters()
        }
    );
}

/// The safepoint rule for blocked threads, at the bare rendezvous: the
/// owner is parked in a wait (as in `thread-join`) and eight workers
/// allocate. One more worker requests a collection and stops the World with
/// no Baton and no turn of the owner; an owner whose wait ends inside the
/// stop (its 10 ms service turn) waits for the stop's end before it runs.
#[test]
fn a_detached_requester_stops_the_world_while_the_owner_is_parked() {
    const WORKERS: usize = 8;
    let stop = Arc::new(StopWorld::new());
    let ready = Arc::new(Barrier::new(WORKERS + 2));
    let release = Arc::new(AtomicBool::new(false));
    // Refreshes of heap mirrors in progress, and done.
    let refreshing = Arc::new(AtomicUsize::new(0));
    let refreshed = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..WORKERS)
        .map(|index| {
            let (stop, ready, release) =
                (Arc::clone(&stop), Arc::clone(&ready), Arc::clone(&release));
            let (refreshing, refreshed) = (Arc::clone(&refreshing), Arc::clone(&refreshed));
            std::thread::spawn(move || {
                let image = || vec![Value::fixnum(index as i64).bits()];
                stop.enter_parked(image());
                stop.resume();
                ready.wait();
                let mut allocated = Vec::new();
                while !release.load(Ordering::Acquire) {
                    allocated.push(index);
                    if allocated.len() == 64 {
                        allocated.clear();
                        if stop.stop_pending() {
                            stop.park_for_stop(image(), || {
                                assert_eq!(
                                    refreshing.fetch_add(1, Ordering::AcqRel),
                                    0,
                                    "one parked thread refreshes at a time"
                                );
                                std::thread::sleep(Duration::from_millis(1));
                                refreshing.fetch_sub(1, Ordering::AcqRel);
                                refreshed.fetch_add(1, Ordering::Release);
                            });
                        }
                    }
                }
                stop.leave();
            })
        })
        .collect();
    let wake_owner = Arc::new(AtomicBool::new(false));
    let owner_turns = Arc::new(AtomicUsize::new(0));
    let owner = {
        let (stop, ready) = (Arc::clone(&stop), Arc::clone(&ready));
        let (wake_owner, owner_turns) = (Arc::clone(&wake_owner), Arc::clone(&owner_turns));
        std::thread::spawn(move || {
            assert!(stop.try_enter_running(), "no stop is in progress");
            // `thread-join`: parked, and so at a safe point.
            stop.leave_running();
            ready.wait();
            while !wake_owner.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            // The service turn's wake (`BatonLock::attach_unparked`).
            if !stop.enter_running_after_stop() && !stop.try_enter_running() {
                stop.enter_running_when_admitted();
            }
            owner_turns.fetch_add(1, Ordering::Release);
            stop.leave_running();
        })
    };
    ready.wait();

    // The requester: this thread, a ninth worker running detached.
    assert!(stop.try_enter_running(), "no stop is in progress");
    assert!(
        stop.stop_from_detached(Vec::new(), || ()),
        "the requester's own stop is established"
    );
    assert_eq!(stop.request(true), Requested::Stopped);
    stop.collect_requested(Requested::Stopped, |roots| {
        assert_eq!(roots.len(), WORKERS, "every worker parked with its image");
        wake_owner.store(true, Ordering::Release);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            owner_turns.load(Ordering::Acquire),
            0,
            "the owner's wake waits for the stop to end"
        );
    });
    assert_eq!(
        refreshed.load(Ordering::Acquire),
        WORKERS,
        "every parked worker refreshed its mirrors inside the stop"
    );
    owner.join().expect("the owner ran after the stop");
    assert_eq!(owner_turns.load(Ordering::Acquire), 1);
    stop.leave_running();
    release.store(true, Ordering::Release);
    for worker in workers {
        worker.join().expect("worker left the stop");
    }
    let counters = stop.counters();
    assert_eq!(counters.collections, 1, "{counters:?}");
    assert_eq!(counters.safepoint_stops, WORKERS as u64, "{counters:?}");
    assert_eq!(counters.waker_waits, 1, "{counters:?}");
    assert!(
        counters.stop_wait_max_ns < 50_000_000,
        "the stop waited for the workers' safe points only: {counters:?}"
    );
}

/// Whether the World's main owner is hosted: a hosted one wakes from its
/// wait every 10 ms for an event-service turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Bare,
    Hosted,
}

/// The owner parked in `thread-join` while eight Lisp threads allocate with
/// a tiny `gc-cons-threshold`: the collections the workers request complete
/// with the owner parked the whole time. The joined thread samples the
/// counters once the owner blocks and again before it ends, so both samples
/// are taken inside the owner's one wait.
fn workers_collect_while_the_owner_waits_in_thread_join(owner: Owner) {
    const COLLECTIONS: u64 = 20;
    let mut ev = parallel();
    let _input = (owner == Owner::Hosted).then(|| {
        let (sender, receiver) = crossbeam_channel::unbounded();
        ev.attach_input_receiver(
            crate::emacs_core::eval::host_input::HostInputReceiver::from_receiver(receiver),
        );
        sender
    });
    // The hosted owner's service turns fall between the collections: the
    // workers churn on until it took a few.
    let turns = if owner == Owner::Hosted { 3 } else { 0 };
    // Set on the heap: a thread's own `setq` of `gc-cons-threshold` reaches
    // the heap only once its owner resolved the GC settings, at the latest
    // at the next collection, which the default threshold puts far away.
    ev.set_gc_threshold(10_000);
    let report = eval(
        &mut ev,
        &format!(
            "(progn
               (setq pgc-main (current-thread) pgc-target nil pgc-done nil)
               (fset 'pgc-turns
                     (lambda ()
                       (plist-get (neomacs-shared-heap-stats) :owner-service-turns)))
               (fset 'pgc-churn
                     (lambda (leader)
                       (let ((n 0))
                         (while (and (< n 200000)
                                     (if leader
                                         (or (< gcs-done pgc-target)
                                             (< (pgc-turns) pgc-turns-target))
                                       (not pgc-done)))
                           (setq n (1+ n))
                           (make-list 50 n))
                         n)))
               (fset 'pgc-stat
                     (lambda (after before key)
                       (- (plist-get after key) (plist-get before key))))
               (let ((gate (make-thread
                            (lambda ()
                              (while (null (thread--blocker pgc-main))
                                (thread-yield))
                              (let ((before (neomacs-shared-heap-stats)))
                                (setq pgc-turns-target (+ (pgc-turns) {turns})
                                      pgc-target (+ gcs-done {COLLECTIONS}))
                                (pgc-churn t)
                                (setq pgc-done t)
                                (let ((after (neomacs-shared-heap-stats)))
                                  (list (pgc-stat after before :collections)
                                        (pgc-stat after before :requester-parks)
                                        (plist-get after :stop-wait-max-ns)))))))
                     (workers nil)
                     (k 0))
                 (while (< k 7)
                   (setq workers
                         (cons (make-thread
                                (lambda ()
                                  (while (null pgc-target) (thread-yield))
                                  (pgc-churn nil)))
                               workers)
                         k (1+ k)))
                 (prog1 (thread-join gate)
                   (mapc #'thread-join workers))))"
        ),
    );
    let numbers: Vec<u64> = report
        .trim_matches(|c| c == '(' || c == ')')
        .split_whitespace()
        .map(|field| field.parse().unwrap_or(u64::MAX))
        .collect();
    let [collections, requester_parks, stop_wait_max_ns] = numbers[..] else {
        panic!("the joined thread reports three counters: {report}");
    };
    // A worker that holds the Baton for a primitive when its collection
    // comes due waits for its peers parked, as any holder does.
    let _ = requester_parks;
    assert!(
        collections >= COLLECTIONS,
        "workers collected while the owner was parked: {report} {:?}",
        stats(&ev)
    );
    assert!(
        stop_wait_max_ns < 50_000_000,
        "no stop waited for a turn of the parked owner: {report}"
    );
    if owner == Owner::Hosted {
        assert!(
            stats(&ev).owner_service_turns > 0,
            "the hosted owner took service turns inside its join"
        );
    }
}

#[test]
fn workers_collect_while_the_owner_waits_in_thread_join_bare() {
    workers_collect_while_the_owner_waits_in_thread_join(Owner::Bare);
}

#[test]
fn workers_collect_while_the_hosted_owner_waits_in_thread_join() {
    workers_collect_while_the_owner_waits_in_thread_join(Owner::Hosted);
}

/// The collector's work spread over a stop's parked threads
/// (`StopWorld::run_on_parked`, the in-stop sweep's executor): every share
/// runs exactly once, and the parked threads run shares while they wait,
/// so none has to be started for them.
#[test]
fn a_stop_runs_each_share_once_on_its_parked_threads() {
    const PARTS: usize = 64;
    let stop = Arc::new(StopWorld::new());
    let ready = Arc::new(Barrier::new(4));
    let release = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..3i64)
        .map(|k| {
            let (stop, ready, release) =
                (Arc::clone(&stop), Arc::clone(&ready), Arc::clone(&release));
            std::thread::spawn(move || {
                stop.enter_parked(vec![Value::fixnum(k).bits()]);
                stop.resume();
                ready.wait();
                while !release.load(Ordering::Acquire) {
                    stop.safepoint(|| vec![Value::fixnum(k).bits()]);
                }
                stop.leave();
            })
        })
        .collect();
    ready.wait();
    let runs: Vec<AtomicUsize> = (0..PARTS).map(|_| AtomicUsize::new(0)).collect();
    let by_parked = AtomicUsize::new(0);
    let collector = std::thread::current().id();
    stop.collect_stopped(|_| {
        stop.run_on_parked(PARTS, &|index| {
            runs[index].fetch_add(1, Ordering::Relaxed);
            if std::thread::current().id() == collector {
                // The collector's share waits until a parked thread has
                // taken one: they were woken for the shares left.
                while by_parked.load(Ordering::Acquire) == 0 {
                    std::thread::yield_now();
                }
            } else {
                by_parked.fetch_add(1, Ordering::Release);
            }
        });
    });
    release.store(true, Ordering::Release);
    for worker in workers {
        worker.join().expect("worker left the stop");
    }
    let runs: Vec<usize> = runs
        .iter()
        .map(|count| count.load(Ordering::Relaxed))
        .collect();
    assert_eq!(runs, vec![1; PARTS], "each share runs once");
    let by_parked = by_parked.load(Ordering::Relaxed);
    assert!(
        (1..PARTS).contains(&by_parked),
        "parked threads ran {by_parked} of {PARTS} shares"
    );
}

/// A share's panic on a parked thread reaches the collector once every
/// share has returned, and the parked threads still resume.
#[test]
fn a_share_panic_on_a_parked_thread_reaches_the_collector() {
    let stop = Arc::new(StopWorld::new());
    let ready = Arc::new(Barrier::new(2));
    let release = Arc::new(AtomicBool::new(false));
    let worker = {
        let (stop, ready, release) = (Arc::clone(&stop), Arc::clone(&ready), Arc::clone(&release));
        std::thread::spawn(move || {
            stop.enter_parked(Vec::new());
            stop.resume();
            ready.wait();
            while !release.load(Ordering::Acquire) {
                stop.safepoint(Vec::new);
            }
            stop.leave();
        })
    };
    ready.wait();
    let collector = std::thread::current().id();
    let finished = AtomicUsize::new(0);
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        stop.collect_stopped(|_| {
            stop.run_on_parked(2, &|_| {
                if std::thread::current().id() == collector {
                    // Hold the collector's share until the other one ran.
                    while finished.load(Ordering::Acquire) == 0 {
                        std::thread::yield_now();
                    }
                } else {
                    finished.store(1, Ordering::Release);
                    panic!("share on a parked thread");
                }
            });
        });
    }));
    release.store(true, Ordering::Release);
    worker.join().expect("the parked thread resumed");
    let panic = caught.expect_err("the share's panic reached the collector");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"share on a parked thread")
    );
    assert!(!ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest));
}

#[test]
fn a_collection_stops_a_thread_running_detached() {
    let mut ev = parallel();
    let inside = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    register_thread_body(
        "pgc-spin",
        spinner(1000, Arc::clone(&inside), Arc::clone(&release)),
    );
    eval(&mut ev, "(setq pgc-th (make-thread 'pgc-spin))");
    yield_until(&mut ev, &inside);
    let before = stats(&ev);
    eval(&mut ev, "(garbage-collect)");
    let after = stats(&ev);
    release.store(true, Ordering::Release);
    assert_eq!(eval(&mut ev, "(thread-join pgc-th)"), "500500");
    assert_eq!(after.collections - before.collections, 1);
    assert_eq!(after.safepoint_stops - before.safepoint_stops, 1);
}

/// Every worker parked for one collection refreshes its own heap mirrors
/// without taking the World's Baton to do so. Only the collector attaches:
/// once for the request, and once after each park it waits in while the
/// workers reach their safe points.
#[test]
fn parked_workers_reinstall_after_collection_without_baton_attaches() {
    const THREADS: usize = 8;
    let mut ev = parallel();
    let inside = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let mut names = Vec::with_capacity(THREADS);
    for index in 0..THREADS {
        let name = Box::leak(format!("pgc-refresh-{index}").into_boxed_str());
        let (inside, release) = (Arc::clone(&inside), Arc::clone(&release));
        register_thread_body(name, move |owner| {
            with_detached(owner, &[], move |section| {
                inside.fetch_add(1, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    section.safepoint();
                    std::thread::yield_now();
                }
                1
            })
        });
        names.push(name);
    }
    let threads = names
        .iter()
        .map(|name| format!("(make-thread '{name})"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        eval(
            &mut ev,
            &format!("(progn (setq pgc-refresh-threads (list {threads})) t)")
        ),
        "t"
    );
    while inside.load(Ordering::Acquire) != THREADS {
        assert_eq!(eval(&mut ev, "(thread-yield)"), "nil");
        std::thread::yield_now();
    }

    let before = stats(&ev);
    eval(&mut ev, "(garbage-collect)");
    let after = stats(&ev);
    assert_eq!(after.collections - before.collections, 1);
    let requester_parks = after.requester_parks - before.requester_parks;
    assert!(
        after.attach_count - before.attach_count <= 1 + requester_parks,
        "parked workers attached to the Baton: {before:?} {after:?}"
    );

    release.store(true, Ordering::Release);
    assert_eq!(
        eval(&mut ev, "(mapcar 'thread-join pgc-refresh-threads)"),
        "(1 1 1 1 1 1 1 1)"
    );
}

/// Eight threads, each building 10_000-element lists in Lisp and summing
/// them in detached sections, with a tiny `gc-cons-threshold`.
fn eight_threads_cons_and_collect() {
    const THREADS: usize = 8;
    const ROUNDS: i64 = 50;
    const N: i64 = 10_000;
    let mut ev = parallel();
    // Threads still consing, and threads waiting in a section for a peer's
    // collection: a section waits for one while some peer still conses, so
    // every run stops threads at their safe points, however it is timed.
    let alive = Arc::new(AtomicUsize::new(THREADS));
    let waiting = Arc::new(AtomicUsize::new(0));
    for k in 0..THREADS {
        let (alive, waiting) = (Arc::clone(&alive), Arc::clone(&waiting));
        register_thread_body(&format!("pgc-cons-{k}"), move |owner| {
            let mut wrong = 0;
            for round in 0..ROUNDS {
                let base = (k as i64 * ROUNDS + round) * 100;
                let list = apply(
                    owner,
                    "pgc-seq",
                    vec![Value::fixnum(N), Value::fixnum(base)],
                );
                let (alive, waiting) = (Arc::clone(&alive), Arc::clone(&waiting));
                let sum = with_detached(owner, &[list], move |section| {
                    let entered = section.collections();
                    waiting.fetch_add(1, Ordering::AcqRel);
                    let mut sum = sum_detached(section, 0);
                    while section.collections() == entered
                        && waiting.load(Ordering::Acquire) < alive.load(Ordering::Acquire)
                    {
                        sum = sum_detached(section, 0);
                    }
                    waiting.fetch_sub(1, Ordering::AcqRel);
                    sum
                });
                if sum != seq_sum(N, base) {
                    wrong += 1;
                }
            }
            alive.fetch_sub(1, Ordering::AcqRel);
            wrong
        });
    }
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq gc-cons-threshold 10000)
               (let ((ths nil) (k 0))
                 (while (< k 8)
                   (setq ths (cons (make-thread (intern (format \"pgc-cons-%d\" k))) ths))
                   (setq k (1+ k)))
                 (mapcar #'thread-join (nreverse ths))))"
        ),
        "(0 0 0 0 0 0 0 0)"
    );
    let stats = stats(&ev);
    println!("eight threads: {stats:?}");
    assert!(stats.collections > 0, "{stats:?}");
    // A stop counts the threads that were inside a detached section; the
    // others were waiting for the Baton (parked by Green's park) or consing
    // under it (see VERIFY.md).
    assert!(stats.safepoint_stops > 0, "{stats:?}");
    assert!(
        stats.safepoint_stops <= stats.collections * THREADS as u64,
        "{stats:?}"
    );
}

#[test]
fn eight_threads_cons_and_collect_generational() {
    assert_ne!(
        std::env::var("NEOVM_GC_GENERATIONAL").as_deref(),
        Ok("0"),
        "the default collector is generational"
    );
    eight_threads_cons_and_collect();
}

#[test]
fn eight_threads_cons_and_collect_full_heap_only() {
    // SAFETY: nextest runs each test in its own process, and no other thread
    // of this one exists yet; the heap reads the variable as it is built.
    unsafe { std::env::set_var("NEOVM_GC_GENERATIONAL", "0") };
    eight_threads_cons_and_collect();
}

#[test]
fn a_collection_completes_while_a_thread_waits_in_thread_join() {
    let mut ev = parallel();
    let inside = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    register_thread_body(
        "pgc-joined",
        spinner(1000, Arc::clone(&inside), Arc::clone(&release)),
    );
    eval(
        &mut ev,
        "(setq pgc-b (make-thread 'pgc-joined)
               pgc-a (make-thread (lambda ()
                                    (let ((kept (pgc-pair 500 7)))
                                      (list (thread-join pgc-b)
                                            (equal (car kept) (cdr kept)))))))",
    );
    yield_until(&mut ev, &inside);
    // Let the joiner reach its join: its body runs detached until it blocks.
    while eval(&mut ev, "(thread--blocker pgc-a)") == "nil" {
        eval(&mut ev, "(thread-yield)");
    }
    let before = stats(&ev);
    eval(
        &mut ev,
        "(let ((j 0)) (while (< j 2000) (make-list 50 j) (setq j (1+ j))) (garbage-collect))",
    );
    let after = stats(&ev);
    release.store(true, Ordering::Release);
    assert_eq!(eval(&mut ev, "(thread-join pgc-a)"), "(500500 t)");
    assert_eq!(eval(&mut ev, "(thread-join pgc-b)"), "500500");
    assert!(after.collections > before.collections);
    // The joiner was parked by its join: only the detached thread stopped.
    assert_eq!(
        after.safepoint_stops - before.safepoint_stops,
        after.collections - before.collections
    );
}

#[test]
fn a_thread_in_condition_wait_survives_a_collection_and_is_notified_after() {
    let mut ev = parallel();
    let inside = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    register_thread_body(
        "pgc-cv-spin",
        spinner(1000, Arc::clone(&inside), Arc::clone(&release)),
    );
    eval(
        &mut ev,
        "(setq pgc-m (make-mutex) pgc-cv (make-condition-variable pgc-m)
               pgc-ready nil pgc-go nil
               pgc-waiter
               (make-thread (lambda ()
                              (let ((kept (pgc-pair 500 0)))
                                (mutex-lock pgc-m)
                                (setq pgc-ready t)
                                (while (null pgc-go) (condition-wait pgc-cv))
                                (mutex-unlock pgc-m)
                                (equal (car kept) (cdr kept)))))
               pgc-spin (make-thread 'pgc-cv-spin))",
    );
    yield_until(&mut ev, &inside);
    // The waiter's body runs detached until it blocks in its wait.
    while eval(&mut ev, "(thread--blocker pgc-waiter)") == "nil" {
        eval(&mut ev, "(thread-yield)");
    }
    let before = stats(&ev);
    eval(
        &mut ev,
        "(let ((j 0)) (while (< j 2000) (make-list 50 j) (setq j (1+ j))) (garbage-collect))",
    );
    assert!(stats(&ev).collections > before.collections);
    assert_eq!(
        eval(
            &mut ev,
            "(progn (mutex-lock pgc-m) (setq pgc-go t) (condition-notify pgc-cv t)
                    (mutex-unlock pgc-m) (thread-join pgc-waiter))"
        ),
        "t"
    );
    release.store(true, Ordering::Release);
    assert_eq!(eval(&mut ev, "(thread-join pgc-spin)"), "500500");
}

#[test]
fn garbage_collect_from_a_child_thread_stops_the_detached_one() {
    let mut ev = parallel();
    let inside = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    register_thread_body(
        "pgc-child-spin",
        spinner(1000, Arc::clone(&inside), Arc::clone(&release)),
    );
    eval(&mut ev, "(setq pgc-spin (make-thread 'pgc-child-spin))");
    yield_until(&mut ev, &inside);
    let before = stats(&ev);
    // Main waits in the join while the child collects.
    assert_eq!(
        eval(
            &mut ev,
            "(thread-join (make-thread (lambda () (garbage-collect) 'collected)))"
        ),
        "collected"
    );
    let after = stats(&ev);
    release.store(true, Ordering::Release);
    assert_eq!(eval(&mut ev, "(thread-join pgc-spin)"), "500500");
    assert!(after.collections > before.collections);
    assert!(after.safepoint_stops > before.safepoint_stops);
}

#[test]
fn values_held_by_detached_threads_equal_their_copies_after_collections() {
    const THREADS: usize = 4;
    let mut ev = parallel();
    let inside = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    for k in 0..THREADS {
        let (inside, release) = (Arc::clone(&inside), Arc::clone(&release));
        register_thread_body(&format!("pgc-live-{k}"), move |owner| {
            let pair = apply(
                owner,
                "pgc-pair",
                vec![Value::fixnum(2000), Value::fixnum(k as i64 * 10_000)],
            );
            let sum = with_detached(owner, &[pair], move |section| {
                inside.fetch_add(1, Ordering::AcqRel);
                while !release.load(Ordering::Acquire) {
                    section.safepoint();
                    std::thread::yield_now();
                }
                // The list itself, read detached after the collections.
                let list = section.held(0).cons_car_unobserved();
                let mut cell = list;
                let mut sum = 0;
                while cell.is_cons() {
                    sum += cell.cons_car_unobserved().as_fixnum().unwrap_or(0);
                    cell = cell.cons_cdr_unobserved();
                }
                sum
            });
            let same = apply(
                owner,
                "equal",
                vec![pair.cons_car_unobserved(), pair.cons_cdr_unobserved()],
            );
            if same.is_nil() || sum != seq_sum(2000, k as i64 * 10_000) {
                -1
            } else {
                1
            }
        });
    }
    eval(
        &mut ev,
        "(setq pgc-live (list (make-thread 'pgc-live-0) (make-thread 'pgc-live-1)
                              (make-thread 'pgc-live-2) (make-thread 'pgc-live-3)))",
    );
    while inside.load(Ordering::Acquire) < THREADS {
        eval(&mut ev, "(thread-yield)");
    }
    let before = stats(&ev);
    // Garbage between the collections reuses whatever a lost root freed.
    eval(
        &mut ev,
        "(let ((i 0))
           (while (< i 3)
             (let ((j 0)) (while (< j 3000) (make-list 20 j) (setq j (1+ j))))
             (garbage-collect)
             (setq i (1+ i))))",
    );
    let after = stats(&ev);
    release.store(true, Ordering::Release);
    assert_eq!(
        eval(&mut ev, "(mapcar #'thread-join pgc-live)"),
        "(1 1 1 1)"
    );
    assert!(after.collections >= before.collections + 3);
    assert_eq!(
        after.safepoint_stops - before.safepoint_stops,
        (after.collections - before.collections) * THREADS as u64
    );
}

#[test]
fn stats_report_the_stop_counters() {
    let mut ev = parallel();
    eval(
        &mut ev,
        "(progn (thread-join (make-thread (lambda () nil))) (garbage-collect))",
    );
    assert_eq!(
        eval(
            &mut ev,
            "(let ((stats (neomacs-shared-heap-stats)))
               (list (plist-get stats :collections)
                     (plist-get stats :safepoint-stops)
                     (natnump (plist-get stats :stop-wait-ns))
                     (natnump (plist-get stats :stop-max-ns))))"
        ),
        "(1 0 t t)"
    );
}

/// Threads parking for a stop wake its waiters once, when the last of them
/// parks: the stop's requester waits for no thread to run, and each parked
/// thread for the stop's end, so a notification at every park only woke
/// every thread parked so far.
#[test]
fn parks_for_a_stop_notify_its_waiters_once_no_thread_runs() {
    const THREADS: usize = 6;
    let stop = Arc::new(StopWorld::new());
    let ready = Arc::new(Barrier::new(THREADS + 1));
    let release = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..THREADS as i64)
        .map(|k| {
            let (stop, ready, release) =
                (Arc::clone(&stop), Arc::clone(&ready), Arc::clone(&release));
            std::thread::spawn(move || {
                stop.enter_parked(vec![Value::fixnum(k).bits()]);
                stop.resume();
                ready.wait();
                while !release.load(Ordering::Acquire) {
                    stop.safepoint(|| vec![Value::fixnum(k).bits()]);
                    std::thread::yield_now();
                }
                stop.leave();
            })
        })
        .collect();
    ready.wait();
    let before = stop.none_running_notifies.load(Ordering::Relaxed);
    let mut by_parks = None;
    stop.collect_stopped(|roots| {
        assert_eq!(roots.len(), THREADS, "every thread parked");
        by_parks = Some(stop.none_running_notifies.load(Ordering::Relaxed) - before);
    });
    release.store(true, Ordering::Release);
    for worker in workers {
        worker.join().expect("worker left the stop");
    }
    assert_eq!(
        by_parks,
        Some(1),
        "{THREADS} parks notified the stop's waiters once"
    );
}
