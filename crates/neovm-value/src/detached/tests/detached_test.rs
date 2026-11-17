//! The detached gate: attach around one access, never twice, and detach on
//! unwind.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static ATTACHES: AtomicUsize = AtomicUsize::new(0);
static DETACHES: AtomicUsize = AtomicUsize::new(0);
static NOTES: AtomicUsize = AtomicUsize::new(0);

unsafe fn attach(_lock: *const (), _reason: AttachReason) -> *mut () {
    ATTACHES.fetch_add(1, Ordering::Relaxed);
    std::ptr::null_mut()
}

unsafe fn detach(_lock: *const (), _token: *mut ()) {
    DETACHES.fetch_add(1, Ordering::Relaxed);
}

unsafe fn note(_lock: *const (), _reason: AttachReason) {
    NOTES.fetch_add(1, Ordering::Relaxed);
}

fn gate() -> DetachedGate {
    DetachedGate {
        lock: std::ptr::null(),
        attach,
        detach,
        note,
    }
}

#[test]
fn a_detached_access_attaches_once_and_detaches_on_unwind() {
    // One test owns the counters: the gate is per OS thread, the counters
    // per process.
    assert!(!is_detached());
    with_attached(AttachReason::Alloc, || ());
    note(AttachReason::Blv);
    assert_eq!(ATTACHES.load(Ordering::Relaxed), 0, "no gate, no attach");
    assert_eq!(NOTES.load(Ordering::Relaxed), 0);

    begin_detached(gate());
    assert!(is_detached() && running_detached());
    let nested = with_attached(AttachReason::Alloc, || {
        assert!(!is_detached(), "attached inside");
        with_attached(AttachReason::Alloc, || 7)
    });
    assert_eq!(nested, 7);
    assert_eq!(
        ATTACHES.load(Ordering::Relaxed),
        1,
        "a nested access is attached already"
    );
    assert_eq!(DETACHES.load(Ordering::Relaxed), 1);
    assert!(is_detached());

    let unwound = std::panic::catch_unwind(|| {
        with_attached(AttachReason::Class, || panic!("test: access unwinds"));
    });
    assert!(unwound.is_err());
    assert_eq!(DETACHES.load(Ordering::Relaxed), 2, "unwinding detaches");
    assert!(is_detached(), "and the gate is back");

    note(AttachReason::Blv);
    assert_eq!(NOTES.load(Ordering::Relaxed), 1);

    let suspended = suspend_detached().expect("detached");
    assert!(!is_detached() && running_detached());
    resume_detached(suspended);
    end_detached();
    assert!(!is_detached() && !running_detached());
}
