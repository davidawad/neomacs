//! The experimental Parallel World (`NEOVM_THREADS=parallel`): the Baton
//! lock itself, then Lisp threads on their own OS threads over one World.

use super::*;
use crate::emacs_core::eval::ThreadingMode;
use crate::emacs_core::print::print_value;
use neovm_value::value::Value;
use std::sync::{Arc, Barrier};

fn lock() -> BatonLock {
    let mut context = Context::new(crate::test_utils::unit_runtime());
    let baton = context
        .issue_green_baton()
        .expect("test World issues its unique Baton");
    BatonLock::new(baton)
}

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

fn world(mode: ThreadingMode) -> Context {
    crate::test_utils::init_test_tracing();
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::force_jit_off_for_test(true);
    Context::new_with_threading(mode, crate::test_utils::unit_runtime())
}

fn parallel() -> Context {
    world(ThreadingMode::Parallel)
}

#[test]
fn attach_releases_the_baton_during_unwind() {
    let lock = Arc::new(lock());
    let worker_lock = Arc::clone(&lock);
    let worker = std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| {
            let _attached = worker_lock.attach();
            panic!("test worker panic while attached");
        });
        assert!(result.is_err());
    });
    worker.join().expect("worker catches its body panic");
    assert!(lock.try_attach().is_some(), "unwind releases the Baton");
}

#[test]
fn contending_attachments_are_serialized_and_counted() {
    let lock = Arc::new(lock());
    let attached = lock.attach();
    let entered = Arc::new(Barrier::new(2));
    let worker_lock = Arc::clone(&lock);
    let worker_entered = Arc::clone(&entered);
    let worker = std::thread::spawn(move || {
        worker_entered.wait();
        let _attached = worker_lock.attach();
    });
    entered.wait();
    drop(attached);
    worker.join().expect("worker attached and detached");

    let stats = lock.stats();
    assert_eq!(stats.attach_count, 2);
    assert!(stats.attach_wait_ns > 0, "contended wait is measured");
    assert!(stats.detached_ns > 0, "detached interval is measured");
}

#[test]
fn attached_state_is_local_to_the_holding_os_thread() {
    let lock = Arc::new(lock());
    let attached = lock.attach();
    assert!(lock.is_attached_here());
    let worker_lock = Arc::clone(&lock);
    let worker = std::thread::spawn(move || worker_lock.is_attached_here());
    assert!(!worker.join().expect("observer thread completed"));
    drop(attached);
    assert!(!lock.is_attached_here());
}

#[test]
fn reentrant_attach_panics_instead_of_deadlocking() {
    let lock = lock();
    let _attached = lock.attach();
    let result = std::panic::catch_unwind(|| lock.attach());
    assert!(result.is_err());
}

#[test]
fn a_yield_lets_a_waiting_owner_attach_first() {
    let lock = Arc::new(lock());
    let baton = lock.attach().into_baton();
    let worker_lock = Arc::clone(&lock);
    let worker = std::thread::spawn(move || {
        let _attached = worker_lock.attach();
    });
    // The worker is either waiting in attach, or has not reached it yet; a
    // yield while it waits must let it in first.
    let mut baton = baton;
    while !worker.is_finished() {
        baton = lock.yield_baton(baton, false);
    }
    worker.join().expect("worker attached");
    lock.detach(baton);
    assert!(lock.stats().attach_count >= 3);
}

#[test]
fn two_threads_counting_under_a_mutex_reach_twenty_thousand() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tp-n 0 tp-m (make-mutex))
               (fset 'tp-count
                     (lambda ()
                       (let ((i 0))
                         (while (< i 10000)
                           (mutex-lock tp-m)
                           (setq tp-n (1+ tp-n))
                           ;; Switch while holding the mutex now and then,
                           ;; so the other thread contends for it.
                           (if (= 0 (% i 1000)) (thread-yield))
                           (mutex-unlock tp-m)
                           (setq i (1+ i))))))
               (let ((a (make-thread 'tp-count))
                     (b (make-thread 'tp-count)))
                 (thread-join a)
                 (thread-join b)
                 tp-n))"
        ),
        "20000"
    );
    // A measurement, printed for the delivery notes; never asserted.
    eprintln!(
        "parallel stats after 2 x 10000 mutex rounds: {}",
        eval(&mut ev, "(neomacs-shared-heap-stats)")
    );
}

#[test]
fn thread_identity_primitives_answer_as_in_green() {
    let form = "(let ((th (make-thread
                           (lambda ()
                             (list (eq (current-thread) main-thread)
                                   (thread-name (current-thread))
                                   (length (all-threads))))
                           \"worker\")))
                  (list (thread-live-p th)
                        (thread-join th)
                        (thread-live-p th)
                        (length (all-threads))
                        (eq (current-thread) main-thread)))";
    let green = eval(&mut world(ThreadingMode::Green), form);
    assert_eq!(green, "(t (nil \"worker\" 2) nil 1 t)");
    assert_eq!(eval(&mut parallel(), form), green);
}

#[test]
fn thread_join_returns_the_value_and_re_raises_a_signal() {
    let mut ev = parallel();
    assert_eq!(
        eval(&mut ev, "(thread-join (make-thread (lambda () (* 6 7))))"),
        "42"
    );
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tpj-m (make-mutex))
               (mutex-lock tpj-m)
               (let ((th (make-thread (lambda ()
                                        (condition-case nil (mutex-lock tpj-m)
                                          (error 'caught))))))
                 (thread-yield)
                 (thread-signal th 'error '(\"y\"))
                 (prog1 (condition-case e (thread-join th) (error (list 'join e)))
                   (mutex-unlock tpj-m))))"
        ),
        "(join (error \"y\"))"
    );
}

#[test]
fn a_thread_error_ends_its_join_as_in_green() {
    let form = "(let ((th (make-thread (lambda () (error \"own\")))))
                  (list (condition-case e (thread-join th) (error (list 'join e)))
                        (thread-last-error)))";
    let green = eval(&mut world(ThreadingMode::Green), form);
    assert_eq!(eval(&mut parallel(), form), green);
}

#[test]
fn a_threads_let_binding_is_its_own() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (defvar tp-v 1)
               (let ((th (make-thread (lambda ()
                                        (let ((tp-v 2))
                                          (thread-yield)
                                          (thread-yield)
                                          tp-v)))))
                 (thread-yield)
                 (let ((seen tp-v))
                   (list seen (thread-join th) tp-v))))"
        ),
        "(1 2 1)"
    );
}

#[test]
fn two_yielding_threads_both_make_progress() {
    let mut ev = parallel();
    // Lock step: each thread waits, yielding, for the other's step. One
    // starving the other would never finish.
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tp-a 0 tp-b 0)
               (let ((a (make-thread
                         (lambda ()
                           (while (< tp-a 200)
                             (setq tp-a (1+ tp-a))
                             (while (< tp-b tp-a) (thread-yield))))))
                     (b (make-thread
                         (lambda ()
                           (while (< tp-b 200)
                             (while (<= tp-a tp-b) (thread-yield))
                             (setq tp-b (1+ tp-b)))))))
                 (thread-join a)
                 (thread-join b)
                 (list tp-a tp-b)))"
        ),
        "(200 200)"
    );
}

#[test]
fn waits_detach_so_sleeping_threads_and_conditions_progress() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tpc-m (make-mutex) tpc-c (make-condition-variable tpc-m)
                     tpc-ready nil)
               (let ((waiter (make-thread (lambda ()
                                            (mutex-lock tpc-m)
                                            (while (null tpc-ready) (condition-wait tpc-c))
                                            (mutex-unlock tpc-m)
                                            'notified)))
                     (sleeper (make-thread (lambda () (sleep-for 0.01) 'slept))))
                 (thread-yield)
                 (mutex-lock tpc-m)
                 (setq tpc-ready t)
                 (condition-notify tpc-c)
                 (mutex-unlock tpc-m)
                 (list (thread-join waiter) (thread-join sleeper))))"
        ),
        "(notified slept)"
    );
}

#[test]
fn the_last_thread_to_block_is_told_that_all_threads_are_blocked() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tpl-m (make-mutex) tpl-c (make-condition-variable tpl-m))
               (thread-join (make-thread (lambda ()
                                           (mutex-lock tpl-m)
                                           (condition-case e (condition-wait tpl-c)
                                             (error (car (cdr e))))))))"
        ),
        "\"All threads are blocked: this one would wait forever (GNU Emacs hangs here; neomacs signals)\""
    );
}

#[test]
fn joins_are_counted_and_contended_attaches_wait() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tps-threads
                     (list (make-thread (lambda () (thread-yield) 'done))
                           (make-thread (lambda () (thread-yield) 'done))
                           (make-thread (lambda () (thread-yield) 'done))
                           (make-thread (lambda () (thread-yield) 'done))))
               (mapcar #'thread-join tps-threads)
               (let ((stats (neomacs-shared-heap-stats)))
                 (list (plist-get stats :mode)
                       (plist-get stats :threads)
                       (>= (plist-get stats :attach-count) 4)
                       (> (plist-get stats :attach-wait-ns) 0)
                       (plist-get stats :safepoint-stops)
                       (plist-get stats :collections)
                       (plist-get stats :deferred-bytes))))"
        ),
        "(parallel 1 t t 0 0 0)"
    );
}

#[test]
fn stats_outside_a_parallel_world_are_zero() {
    let form = "(neomacs-shared-heap-stats)";
    assert_eq!(
        eval(&mut world(ThreadingMode::Sim), form),
        "(:mode sim :threads 1 :attach-count 0 :attach-wait-ns 0 :detached-ns 0 \
         :safepoint-stops 0 :collections 0 :deferred-bytes 0 \
         :attach-by-class 0 :attach-by-alloc 0 :attach-by-intern 0 :attach-by-blv 0 \
         :stop-wait-ns 0 :stop-max-ns 0)"
    );
    let mut green = world(ThreadingMode::Green);
    assert_eq!(
        eval(
            &mut green,
            "(progn (thread-join (make-thread (lambda () nil)))
                    (neomacs-shared-heap-stats))"
        ),
        "(:mode green :threads 1 :attach-count 0 :attach-wait-ns 0 :detached-ns 0 \
         :safepoint-stops 0 :collections 0 :deferred-bytes 0 \
         :attach-by-class 0 :attach-by-alloc 0 :attach-by-intern 0 :attach-by-blv 0 \
         :stop-wait-ns 0 :stop-max-ns 0)"
    );
}

thread_local! {
    static TEST_MAIN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs in a Lisp thread: it holds the Baton on an OS thread of its own.
fn attached_off_main(context: &mut Context) -> neovm_value::error::EvalResult {
    let off_main = !TEST_MAIN.with(std::cell::Cell::get);
    Ok(Value::bool_val(off_main && context.attached()))
}

#[test]
fn a_running_thread_holds_the_baton_on_its_own_os_thread() {
    TEST_MAIN.with(|main| main.set(true));
    let mut ev = parallel();
    ev.register_subr(crate::emacs_core::subr::SubrSpec::fixed0(
        "tp-attached-off-main",
        attached_off_main,
    ));
    assert_eq!(
        eval(
            &mut ev,
            "(thread-join (make-thread (lambda () (tp-attached-off-main))))"
        ),
        "t"
    );
    // Main runs again: it is the holder, and no other OS thread is.
    assert!(ev.attached());
    let lock = ev.parallel_lock_shared().expect("a started Parallel World");
    let observer = std::thread::spawn(move || lock.is_attached_here());
    assert!(!observer.join().expect("observer thread completed"));
    TEST_MAIN.with(|main| main.set(false));
}

#[test]
fn a_panic_in_a_thread_body_releases_the_baton() {
    let mut ev = parallel();
    eval(&mut ev, "(thread-join (make-thread (lambda () nil)))");
    crate::emacs_core::eval::green::switch::PANIC_NEXT_BODY
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((th (make-thread (lambda () 'unreached))))
               (list (thread-join th) (thread-last-error) (thread-live-p th)))"
        ),
        "(nil (error \"Lisp thread panicked\") nil)"
    );
    assert_eq!(
        eval(&mut ev, "(thread-join (make-thread (lambda () 'again)))"),
        "again"
    );
}

#[test]
fn a_detached_threads_values_survive_another_threads_collection() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(let ((kept (make-list 1000 'kept)))
               (thread-join
                (make-thread (lambda ()
                               (let ((i 0))
                                 (while (< i 2000)
                                   (make-list 50 i)
                                   (setq i (1+ i))))
                               (garbage-collect))))
               (list (length kept) (car kept) (nth 999 kept)))"
        ),
        "(1000 kept kept)"
    );
}

#[test]
fn eight_threads_yield_and_collect_without_losing_values() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (fset 'tp8-run
                     (lambda (n)
                       (let ((mine nil) (k 0))
                         (while (< k 30)
                           (setq mine (cons (make-list 20 (+ (* n 1000) k)) mine))
                           (thread-yield)
                           (if (= (% k 10) 0) (garbage-collect))
                           (setq k (1+ k)))
                         (let ((sum 0))
                           (while mine
                             (let ((xs (car mine)))
                               (while xs (setq sum (+ sum (car xs)) xs (cdr xs))))
                             (setq mine (cdr mine)))
                           sum))))
               (let ((ths nil) (i 0))
                 (while (< i 8)
                   (setq ths (cons (make-thread (list 'lambda nil (list 'tp8-run i))) ths))
                   (setq i (1+ i)))
                 (mapcar #'thread-join (nreverse ths))))"
        ),
        "(8700 608700 1208700 1808700 2408700 3008700 3608700 4208700)"
    );
}

#[test]
fn a_detached_threads_vm_frames_survive_other_threads_collections() {
    let mut ev = parallel();
    // `(lambda () (list (make-list 100 'x) (tpv-next) (make-list 100 'y)))`
    // as byte-code: its first list lives only on the VM stack across the call.
    eval(
        &mut ev,
        "(fset 'tpv-f
           (make-byte-code nil
                           (unibyte-string 192 193 194 195 34 196 32 193 194 197 34 35 135)
                           [list make-list 100 x tpv-next y]
                           6))",
    );
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tpv-depth 10)
               (fset 'tpv-next (lambda ()
                                 (if (> tpv-depth 0)
                                     (progn (setq tpv-depth (1- tpv-depth)) (tpv-f))
                                   (thread-yield)
                                   'bottom)))
               (fset 'tpv-count (lambda (r)
                                  (let ((n 0))
                                    (while (consp r)
                                      (let ((xs (car r)))
                                        (while xs
                                          (if (eq (car xs) 'x) (setq n (1+ n)))
                                          (setq xs (cdr xs))))
                                      (setq r (car (cdr r))))
                                    n)))
               (let ((th (make-thread (lambda () (tpv-count (tpv-f))))))
                 (thread-yield)
                 (let ((i 0))
                   (while (< i 3)
                     (let ((j 0)) (while (< j 3000) (make-list 20 j) (setq j (1+ j))))
                     (garbage-collect)
                     (setq i (1+ i))))
                 (thread-join th)))"
        ),
        "1100"
    );
}

#[test]
fn main_ending_ends_every_thread_and_joins_its_os_thread() {
    let mut ev = parallel();
    eval(
        &mut ev,
        "(progn
           (setq tpt-m (make-mutex) tpt-ran nil)
           (mutex-lock tpt-m)
           (setq tpt-a (make-thread (lambda ()
                                      (unwind-protect (mutex-lock tpt-m)
                                        (setq tpt-ran 'a)))))
           (setq tpt-b (make-thread (lambda ()
                                      (unwind-protect (sleep-for 30)
                                        (setq tpt-ran 'b)))))
           (thread-yield)
           (setq tpt-c (make-thread (lambda () (setq tpt-ran 'c)))))",
    );
    ev.green_teardown();
    assert!(ev.visible_variable_value_or_nil("tpt-ran").is_nil());
    assert_eq!(
        eval(
            &mut ev,
            "(list (thread-live-p tpt-a) (thread-live-p tpt-b) (thread-live-p tpt-c)
                   (plist-get (neomacs-shared-heap-stats) :threads))"
        ),
        "(nil nil nil 1)"
    );
    drop(ev);
}
