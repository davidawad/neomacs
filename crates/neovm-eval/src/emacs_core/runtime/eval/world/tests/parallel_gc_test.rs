//! Stop-the-world collection across a Parallel World's threads: the
//! rendezvous itself, then Lisp threads that run detached sections (the
//! test-only stand-in for S3's detached execution) while other threads
//! collect.

use super::detached::{register_thread_body, with_detached};
use super::{StopCounters, StopWorld};
use crate::emacs_core::eval::{Context, ThreadingMode};
use crate::emacs_core::print::print_value;
use neovm_value::quit::{ASYNC_ATTENTION, AsyncSource};
use neovm_value::value::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

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
