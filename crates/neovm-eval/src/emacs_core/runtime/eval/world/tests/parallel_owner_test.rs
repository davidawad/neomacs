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
         :safepoint-stops 0 :collections 0 :deferred-bytes 0)"
    );
    let mut green = world(ThreadingMode::Green);
    assert_eq!(
        eval(
            &mut green,
            "(progn (thread-join (make-thread (lambda () nil)))
                    (neomacs-shared-heap-stats))"
        ),
        "(:mode green :threads 1 :attach-count 0 :attach-wait-ns 0 :detached-ns 0 \
         :safepoint-stops 0 :collections 0 :deferred-bytes 0)"
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
