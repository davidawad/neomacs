//! Collections while a Parallel World's threads run detached: a thread
//! whose allocation makes a collection due requests it, every other
//! detached thread parks at its next exact safe point, and the requester
//! waits for them without the Baton, so a thread blocked attaching (for a
//! `World`-class primitive or a heap access) is let through first.

use super::*;
use crate::emacs_core::eval::ThreadingMode;
use crate::emacs_core::print::print_value;
use neovm_value::value::Value;

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

fn parallel() -> Context {
    crate::test_utils::init_test_tracing();
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::force_jit_off_for_test(true);
    let mut ev =
        Context::new_with_threading(ThreadingMode::Parallel, crate::test_utils::unit_runtime());
    ev.set_lexical_binding(true);
    ev
}

fn stats_so_far(ev: &Context) -> ParallelStats {
    ev.parallel_lock().map(BatonLock::stats).unwrap_or_default()
}

/// `(tpc-cons N)`: cons N fixnums onto a list and return
/// `(FIRST LENGTH COLLECTIONS)`, COLLECTIONS being the growth of
/// `gcs-done` while it consed. No `World`-class primitive runs in the loop.
const TPC_CONS: &str = "(defalias 'tpc-cons
  (lambda (n)
    (let ((i 0) (l nil) (g gcs-done))
      (while (< i n)
        (setq l (cons i l))
        (setq i (1+ i)))
      (list (car l) (length l) (- gcs-done g)))))";

/// One detached thread conses past several collection thresholds: it
/// requests the collections itself, while it runs, and its list survives
/// every one of them.
#[test]
fn a_consing_detached_thread_collects_while_it_runs() {
    let mut ev = parallel();
    eval(&mut ev, TPC_CONS);
    let before = stats_so_far(&ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((r (thread-join (make-thread (lambda () (tpc-cons 300000))))))
               (list (car r) (car (cdr r)) (> (car (cdr (cdr r))) 0)))"
        ),
        "(299999 300000 t)"
    );
    let after = stats_so_far(&ev);
    assert!(after.collections > before.collections, "{after:?}");
}

/// A detached thread that never allocates parks at its exact safe points
/// for the collections another detached thread requests: every one of
/// them stops it (it runs from before the consing thread starts until
/// after it ends), and both threads' results are intact.
#[test]
fn a_collection_stops_a_detached_spinner_at_its_safe_points() {
    let mut ev = parallel();
    eval(&mut ev, TPC_CONS);
    eval(
        &mut ev,
        "(progn (defvar tpc-started nil) (defvar tpc-stop nil))",
    );
    let before = stats_so_far(&ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let* ((spinner (make-thread
                              (lambda ()
                                (setq tpc-started t)
                                (let ((n 0))
                                  (while (null tpc-stop) (setq n (1+ n)))
                                  (> n 0)))))
                    (consed (progn
                              (while (null tpc-started) (thread-yield))
                              (thread-join
                               (make-thread (lambda () (tpc-cons 300000)))))))
               (setq tpc-stop t)
               (list (car consed) (car (cdr consed))
                     (> (car (cdr (cdr consed))) 0)
                     (thread-join spinner)))"
        ),
        "(299999 300000 t t)"
    );
    let after = stats_so_far(&ev);
    let collections = after.collections - before.collections;
    let stops = after.safepoint_stops - before.safepoint_stops;
    println!("collections {collections}, safe-point stops {stops}: {after:?}");
    assert!(collections > 0, "{after:?}");
    assert!(
        stops >= collections,
        "the spinner ran through all {collections} collections: {stops} stops"
    );
}

/// Four consing threads, two of which also attach once per iteration (a
/// `World`-class `format`): collections run while they run, a thread
/// waiting to attach gets the Baton while a requester waits for the
/// others, and every thread's list and string count are intact.
#[test]
fn collections_during_detached_execution_keep_every_threads_data() {
    let mut ev = parallel();
    eval(&mut ev, TPC_CONS);
    eval(
        &mut ev,
        "(defalias 'tpc-format
           (lambda (n)
             (let ((i 0) (l nil) (g gcs-done))
               (while (< i n)
                 (setq l (cons (format \"%d\" i) l))
                 (setq i (1+ i)))
               (list (car l) (length l) (- gcs-done g)))))",
    );
    let before = stats_so_far(&ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((ts (list (make-thread (lambda () (tpc-cons 200000)))
                             (make-thread (lambda () (tpc-format 20000)))
                             (make-thread (lambda () (tpc-cons 200000)))
                             (make-thread (lambda () (tpc-format 20000))))))
               (let ((rs (mapcar 'thread-join ts)))
                 (list (mapcar (lambda (r) (list (car r) (car (cdr r)))) rs)
                       ;; Collections ran inside the threads.
                       (> (apply '+ (mapcar (lambda (r) (car (cdr (cdr r)))) rs))
                          0))))"
        ),
        "(((199999 200000) (\"19999\" 20000) (199999 200000) (\"19999\" 20000)) t)"
    );
    let after = stats_so_far(&ev);
    assert!(after.collections > before.collections, "{after:?}");
}

/// `(tpc-garbage N)`: cons N cells, each garbage once the next is made, and
/// return the last one's car.
const TPC_GARBAGE: &str = "(defalias 'tpc-garbage
  (lambda (n)
    (let ((i 0) (c nil))
      (while (< i n)
        (setq c (cons i nil))
        (setq i (1+ i)))
      (car c))))";

/// `(tpc-slow-garbage N K)`: `tpc-garbage` with K loop steps after each
/// cons, which slow its consing down.
const TPC_SLOW_GARBAGE: &str = "(defalias 'tpc-slow-garbage
  (lambda (n k)
    (let ((i 0) (j 0) (c nil))
      (while (< i n)
        (setq c (cons i nil))
        (setq j 0)
        (while (< j k) (setq j (1+ j)))
        (setq i (1+ i)))
      (car c))))";

/// Two detached threads cons garbage at once, one about four times as fast
/// as the other and four times as much. Between two collections every
/// owner that allocates allocates a threshold, as a thread alone does, so
/// the pair runs about half the collections main runs consing the same
/// alone. Paced on its own bytes, the fast owner would collect at each
/// threshold of its own while the slow one had allocated a quarter of its
/// own (owners resuming from a collection one after another start
/// unevenly the same way), about four fifths of main's collections.
#[test]
fn owners_allocating_together_collect_once_per_threshold_each() {
    let mut ev = parallel();
    eval(&mut ev, TPC_GARBAGE);
    eval(&mut ev, TPC_SLOW_GARBAGE);
    assert_eq!(
        eval(&mut ev, "(thread-join (make-thread (lambda () t)))"),
        "t"
    );
    let alone: u64 = eval(
        &mut ev,
        "(let ((g gcs-done))
           (tpc-garbage 1600000)
           (tpc-slow-garbage 400000 4)
           (- gcs-done g))",
    )
    .parse()
    .expect("a count");
    let before = stats_so_far(&ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((slow (make-thread (lambda () (tpc-slow-garbage 400000 4))))
                   (fast (make-thread (lambda () (tpc-garbage 1600000)))))
               (list (thread-join fast) (thread-join slow)))"
        ),
        "(1599999 399999)"
    );
    let together = stats_so_far(&ev).collections - before.collections;
    println!("main alone: {alone} collections; the two threads: {together}");
    assert!(alone >= 20, "main's run collects: {alone}");
    assert!(
        together * 3 <= alone * 2,
        "{together} collections for two owners, {alone} for main alone"
    );
}
