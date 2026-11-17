use super::*;
use std::sync::{Arc, Barrier};

fn lock() -> BatonLock {
    let mut context = Context::new(crate::test_utils::unit_runtime());
    let baton = context
        .issue_green_baton()
        .expect("test World issues its unique Baton");
    BatonLock::new(baton)
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
