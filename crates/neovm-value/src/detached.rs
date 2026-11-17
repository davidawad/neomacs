//! A Parallel World's detached OS thread (feature `shared-heap`).
//!
//! A Lisp thread of a Parallel World runs its Scalar- and Sealed-class code
//! without the World's Baton. While it does, this crate must not hand it the
//! shared heap: every OS thread's [`with_tagged_heap`] reaches the one
//! `TaggedHeap` by `&mut`. So the evaluator marks the OS thread detached
//! here, with a [`DetachedGate`] naming how to attach, and this crate
//! attaches for the duration of every heap access the thread makes while
//! detached, except an allocation: the thread's first one attaches to make
//! the thread's own allocation runs (`tagged::gc::owner_pages`), and every
//! later one takes its object from them without the Baton. The runs close
//! whenever the thread seats the Baton (only then may a collection run) and
//! when its body ends.
//!
//! The gate is per OS thread and is installed only by the evaluator's
//! Parallel thread body: an OS thread without one (every thread of a Sim or
//! Green World) never takes a branch past the slot's load.
//!
//! [`with_tagged_heap`]: crate::tagged::gc::with_tagged_heap

use crate::tagged::gc::OwnerAllocState;
use std::cell::Cell;
use std::ptr::NonNull;

/// Why a detached thread attached (or, for [`AttachReason::Blv`], read
/// shared state on the detached path): one counter per reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum AttachReason {
    /// A `World`-class primitive.
    Class,
    /// A heap access: an allocation, or a primitive whose class would run
    /// detached but which allocates or writes heap metadata.
    Alloc,
    /// A primitive that interns.
    Intern,
    /// A buffer-local read that would have swapped the shared BLV cache.
    Blv,
}

/// How a detached OS thread attaches to its World's Baton lock. The
/// evaluator owns the lock; this crate sees it only through these entries.
#[derive(Clone, Copy, Debug)]
#[doc(hidden)]
pub struct DetachedGate {
    /// The World's lock, erased.
    lock: *const (),
    /// Attach for REASON, blocking; returns the token `detach` takes back.
    attach: unsafe fn(lock: *const (), reason: AttachReason) -> *mut (),
    /// Detach again with the token `attach` returned.
    detach: unsafe fn(lock: *const (), token: *mut ()),
    /// Count REASON without attaching.
    note: unsafe fn(lock: *const (), reason: AttachReason),
    /// Defer a collection due at BYTES, without attaching.
    defer: unsafe fn(lock: *const (), bytes: u64),
}

impl DetachedGate {
    /// The gate of LOCK, an erased lock the four entries take back. LOCK
    /// must outlive every thread that runs detached behind the gate.
    #[doc(hidden)]
    pub fn new(
        lock: *const (),
        attach: unsafe fn(lock: *const (), reason: AttachReason) -> *mut (),
        detach: unsafe fn(lock: *const (), token: *mut ()),
        note: unsafe fn(lock: *const (), reason: AttachReason),
        defer: unsafe fn(lock: *const (), bytes: u64),
    ) -> Self {
        Self {
            lock,
            attach,
            detach,
            note,
            defer,
        }
    }
}

/// This OS thread's place in a Parallel World.
#[derive(Clone, Copy, Debug)]
struct Placement {
    /// The gate while the thread runs detached; `None` while it holds the
    /// Baton (or is in no Parallel World).
    gate: Option<DetachedGate>,
    /// Whether the thread is inside its detached body at all, attached for
    /// a moment or not: its stack holds roots no collector can see.
    running_detached: bool,
    /// The gate and binding depth while the thread stays attached for a
    /// dynamic binding of a shared cell ([`pin_at`]).
    pinned: Option<(DetachedGate, usize)>,
    /// The detached body's own allocation runs, from its first allocation
    /// to its end ([`with_owner_allocation`]); installed while the thread
    /// runs detached, closed and uninstalled while it holds the Baton.
    owner: Option<NonNull<OwnerAllocState>>,
}

impl Placement {
    const OUTSIDE: Self = Self {
        gate: None,
        running_detached: false,
        pinned: None,
        owner: None,
    };
}

thread_local! {
    static PLACEMENT: Cell<Placement> = const { Cell::new(Placement::OUTSIDE) };
}

fn placement() -> Placement {
    PLACEMENT.with(Cell::get)
}

fn set_gate(gate: Option<DetachedGate>) {
    PLACEMENT.with(|slot| {
        slot.set(Placement { gate, ..slot.get() });
    });
}

/// Whether this OS thread runs detached now (without the Baton).
#[doc(hidden)]
#[inline(always)]
pub fn is_detached() -> bool {
    PLACEMENT.with(|slot| slot.get().gate.is_some())
}

/// Whether this OS thread is inside a detached body, attached for a
/// moment or not.
#[doc(hidden)]
#[inline]
pub fn running_detached() -> bool {
    placement().running_detached
}

/// The thread's body starts running detached through GATE.
#[doc(hidden)]
pub fn begin_detached(gate: DetachedGate) {
    PLACEMENT.with(|slot| {
        slot.set(Placement {
            gate: Some(gate),
            running_detached: true,
            ..Placement::OUTSIDE
        });
    });
}

/// The thread's body ends; it holds the Baton again (or is about to
/// attach for its epilogue). Its allocation runs close for good.
#[doc(hidden)]
pub fn end_detached() {
    if let Some(owner) = placement().owner {
        park_owner(owner);
        // SAFETY: made by `with_owner_allocation` from a `Box` this
        // placement alone held; uninstalled above, and forgotten below.
        drop(unsafe { Box::from_raw(owner.as_ptr()) });
    }
    PLACEMENT.with(|slot| slot.set(Placement::OUTSIDE));
}

/// Close OWNER's runs and uninstall it: the thread is about to hold the
/// Baton, and a collection may run then, which must find every run closed.
fn park_owner(owner: NonNull<OwnerAllocState>) {
    crate::tagged::gc::set_installed_owner(std::ptr::null_mut());
    // SAFETY: this thread's own state (`with_owner_allocation`), no longer
    // installed, so no allocation is using it.
    crate::tagged::gc::close_owner_regions(unsafe { &mut *owner.as_ptr() });
}

/// The thread attached for a while: it holds the Baton until
/// [`resume_detached`]. Returns the gate to resume with.
#[doc(hidden)]
pub fn suspend_detached() -> Option<DetachedGate> {
    let placement = placement();
    let gate = placement.gate?;
    if let Some(owner) = placement.owner {
        park_owner(owner);
    }
    set_gate(None);
    Some(gate)
}

/// The thread detaches again through GATE; its allocation runs, if it has
/// any, reopen at its next allocation.
#[doc(hidden)]
pub fn resume_detached(gate: DetachedGate) {
    if let Some(owner) = placement().owner {
        crate::tagged::gc::set_installed_owner(owner.as_ptr());
    }
    set_gate(Some(gate));
}

/// The detached thread binds a shared cell dynamically at binding depth
/// DEPTH: it stays attached until the binding unwinds ([`unpin_at`]).
/// False (and nothing changes) when the thread does not run detached; when
/// true, the caller attaches.
#[doc(hidden)]
pub fn pin_at(depth: usize) -> bool {
    let placement = placement();
    let Some(gate) = placement.gate else {
        return false;
    };
    if let Some(owner) = placement.owner {
        park_owner(owner);
    }
    PLACEMENT.with(|slot| {
        slot.set(Placement {
            gate: None,
            pinned: Some((gate, depth)),
            ..placement
        });
    });
    true
}

/// The thread's bindings unwound to DEPTH: when that ends a pin, the gate
/// to detach with (the caller detaches, then [`resume_detached`]).
#[doc(hidden)]
#[inline]
pub fn unpin_at(depth: usize) -> Option<DetachedGate> {
    let placement = placement();
    let (gate, pinned) = placement.pinned?;
    if depth > pinned {
        return None;
    }
    PLACEMENT.with(|slot| {
        slot.set(Placement {
            pinned: None,
            ..placement
        });
    });
    Some(gate)
}

/// Count REASON on the thread's World when the thread runs detached.
#[doc(hidden)]
#[inline]
pub fn note(reason: AttachReason) {
    if let Some(gate) = placement().gate {
        // SAFETY: the evaluator installed GATE with its World's live lock,
        // which outlives the thread's detached body.
        unsafe { (gate.note)(gate.lock, reason) };
    }
}

/// Owners' allocations made a collection due at BYTES: when this OS thread
/// runs detached, its World defers one, as an attached access that made it
/// due would (counted once per deferred collection).
pub(crate) fn defer_collection(bytes: u64) {
    if let Some(gate) = placement().gate {
        // SAFETY: as in `note`.
        unsafe { (gate.defer)(gate.lock, bytes) };
    }
}

/// Run F, which allocates through the entry points that route to an
/// installed owner state, on this detached OS thread's own allocation
/// runs: without the Baton once the thread has them. The thread's first
/// allocation attaches to make them (MAKE runs attached) and allocates
/// attached. The caller has checked [`is_detached`].
#[inline]
pub(crate) fn with_owner_allocation<R>(
    make: impl FnOnce() -> NonNull<OwnerAllocState>,
    f: impl FnOnce() -> R,
) -> R {
    let placement = placement();
    debug_assert!(placement.gate.is_some(), "only a detached thread");
    if placement.owner.is_some() {
        return f();
    }
    with_attached(AttachReason::Alloc, || {
        let owner = make();
        crate::tagged::gc::set_installed_owner(owner.as_ptr());
        PLACEMENT.with(|slot| {
            slot.set(Placement {
                owner: Some(owner),
                ..slot.get()
            });
        });
        f()
    })
}

/// Holds the Baton for a scope when this OS thread runs detached: entering
/// attaches for a reason, dropping (also on unwind) detaches and
/// reinstalls the gate. A no-op on an attached thread.
#[doc(hidden)]
#[must_use = "dropping the scope detaches at once"]
pub struct AttachedScope(Option<(DetachedGate, *mut ())>);

impl AttachedScope {
    /// Attach for REASON if this OS thread runs detached.
    #[doc(hidden)]
    #[inline]
    pub fn enter(reason: AttachReason) -> Self {
        let Some(gate) = placement().gate else {
            return Self(None);
        };
        set_gate(None);
        // SAFETY: as in `note`; the gate is cleared first, so accesses made
        // inside the scope do not attach again.
        let token = unsafe { (gate.attach)(gate.lock, reason) };
        Self(Some((gate, token)))
    }
}

impl Drop for AttachedScope {
    fn drop(&mut self) {
        if let Some((gate, token)) = self.0.take() {
            // SAFETY: TOKEN came from this gate's `attach`, once.
            unsafe { (gate.detach)(gate.lock, token) };
            set_gate(Some(gate));
        }
    }
}

/// Run F holding the Baton when this OS thread runs detached (attaching
/// for REASON and detaching after, also when F unwinds), else just run F.
#[doc(hidden)]
#[inline]
pub fn with_attached<R>(reason: AttachReason, f: impl FnOnce() -> R) -> R {
    let _scope = AttachedScope::enter(reason);
    f()
}

#[cfg(test)]
#[path = "detached/tests/detached_test.rs"]
mod tests;
