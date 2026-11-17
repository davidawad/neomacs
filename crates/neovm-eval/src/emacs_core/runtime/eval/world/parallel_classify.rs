//! Detached execution in a Parallel World (feature `shared-heap`, slice
//! S3): which primitives a Lisp thread may run without the Baton, and the
//! seams that attach for the others.
//!
//! A Parallel Lisp thread runs its body detached. The interpreter and
//! bytecode loops, special forms, `funcall`/`apply`/`eval`, closures,
//! owner-slab bindings and canonical value reads (Acquire loads of the
//! value word) touch no World state the Baton guards. A primitive does,
//! unless the effect inventory classifies it `Scalar` or `Sealed`
//! ([`ParallelClass`]); every other primitive runs attached: the subr seam
//! takes the Baton for the call and puts it back after. Heap accesses the
//! thread makes while detached attach in `neovm_value::detached`, and
//! collections wait until no thread runs detached.

use super::{Baton, BatonLock, Context};
use crate::emacs_core::eval::subr_abi::SubrFn;
use crate::emacs_core::subr::effect_catalog::inventory;
use crate::emacs_core::subr::effect_schema::{
    ArithOperands, CatalogKind, ParallelClass, ParallelVariant,
};
use neovm_value::detached::{AttachReason, DetachedGate};
use neovm_value::error::Flow;
use neovm_value::intern::SymId;
use neovm_value::value::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// A detached thread attached for one primitive
/// ([`Context::parallel_attach`]); give it to [`Context::parallel_detach`].
#[must_use = "the thread holds the Baton until it detaches"]
pub(crate) struct ParallelAttach {
    lock: std::sync::Arc<BatonLock>,
    gate: DetachedGate,
}

/// Where a primitive runs in a detached thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Placement {
    /// Detached, whatever its arguments.
    Detached,
    /// Detached when every argument is a number (fixnum, float, bignum); a
    /// marker argument is a buffer read, and attaches.
    DetachedIfNumeric,
    /// Attached, for the reason counted.
    Attach(AttachReason),
}

/// Primitives that run attached although their class would let them run
/// detached, or whose attach is counted under a reason more precise than
/// their class. Each row: name, reason, why.
const ATTACH_OVERRIDES: &[(&str, AttachReason, &str)] = &[
    (
        "setcar",
        AttachReason::Alloc,
        "Sealed, but the generational barrier writes the shared heap's card table",
    ),
    (
        "setcdr",
        AttachReason::Alloc,
        "Sealed, but the generational barrier writes the shared heap's card table",
    ),
    (
        "intern",
        AttachReason::Intern,
        "adds a symbol to the shared obarray",
    ),
    (
        "intern-soft",
        AttachReason::Intern,
        "reads the obarray an intern may be resizing",
    ),
    (
        "read",
        AttachReason::Intern,
        "interns every symbol it reads",
    ),
    (
        "read-from-string",
        AttachReason::Intern,
        "interns every symbol it reads",
    ),
    (
        "unintern",
        AttachReason::Intern,
        "removes a symbol from the shared obarray",
    ),
    (
        "make-string",
        AttachReason::Alloc,
        "allocates a string; per-owner string pages are slice S5",
    ),
    (
        "string",
        AttachReason::Alloc,
        "allocates a string (slice S5)",
    ),
    (
        "concat",
        AttachReason::Alloc,
        "allocates a string (slice S5)",
    ),
    (
        "substring",
        AttachReason::Alloc,
        "allocates a string (slice S5)",
    ),
    (
        "format",
        AttachReason::Alloc,
        "allocates a string (slice S5)",
    ),
    (
        "number-to-string",
        AttachReason::Alloc,
        "allocates a string (slice S5)",
    ),
    (
        "symbol-name",
        AttachReason::Alloc,
        "may materialize a name string (slice S5)",
    ),
    (
        "make-vector",
        AttachReason::Alloc,
        "allocates a vector (slice S5)",
    ),
    (
        "vector",
        AttachReason::Alloc,
        "allocates a vector (slice S5)",
    ),
    (
        "vconcat",
        AttachReason::Alloc,
        "allocates a vector (slice S5)",
    ),
    (
        "make-bool-vector",
        AttachReason::Alloc,
        "allocates a vector (slice S5)",
    ),
    (
        "make-hash-table",
        AttachReason::Alloc,
        "allocates a hash table (slice S5)",
    ),
    (
        "copy-hash-table",
        AttachReason::Alloc,
        "allocates a hash table (slice S5)",
    ),
    (
        "puthash",
        AttachReason::Alloc,
        "may grow a hash table (slice S5)",
    ),
    (
        "make-record",
        AttachReason::Alloc,
        "allocates a record (slice S5)",
    ),
    (
        "record",
        AttachReason::Alloc,
        "allocates a record (slice S5)",
    ),
    (
        "make-closure",
        AttachReason::Alloc,
        "allocates a closure (slice S5)",
    ),
    (
        "make-byte-code",
        AttachReason::Alloc,
        "allocates a closure (slice S5)",
    ),
    (
        "make-marker",
        AttachReason::Alloc,
        "allocates a marker (slice S5)",
    ),
    (
        "copy-sequence",
        AttachReason::Alloc,
        "allocates a sequence (slice S5)",
    ),
];

/// Primitives with no inventory declaration (so `World` by default) that are
/// pure functions of their numeric arguments, as the inventory's own
/// `Arith(Numeric)` split says of `+` and its family: they run detached
/// for numeric arguments. A bignum or float result allocates, and that
/// heap access attaches. Each row: name, why.
const DETACHED_NUMERIC: &[(&str, &str)] = &[
    ("logand", "bitwise AND of integers; no state"),
    ("logior", "bitwise OR of integers; no state"),
    ("logxor", "bitwise XOR of integers; no state"),
    ("lognot", "bitwise NOT of an integer; no state"),
    ("ash", "arithmetic shift of an integer; no state"),
];

/// Evaluator entry points the inventory leaves `World` by default: they
/// run their function, whose own primitives meet the seam. Each row: name,
/// why.
const DETACHED_EVALUATOR: &[(&str, &str)] = &[
    (
        "funcall",
        "calls its function; the callee's primitives meet the seam",
    ),
    (
        "apply",
        "calls its function; the callee's primitives meet the seam",
    ),
    (
        "eval",
        "evaluates its form; the form's primitives meet the seam",
    ),
];

/// The placement of the primitive NAME.
pub(crate) fn placement(name: &str) -> Placement {
    if let Some(&(_, reason, _)) = ATTACH_OVERRIDES.iter().find(|row| row.0 == name) {
        return Placement::Attach(reason);
    }
    if DETACHED_NUMERIC.iter().any(|row| row.0 == name) {
        return Placement::DetachedIfNumeric;
    }
    if DETACHED_EVALUATOR.iter().any(|row| row.0 == name) {
        return Placement::Detached;
    }
    let entry = inventory()
        .iter()
        .find(|entry| entry.kind == CatalogKind::Primitive && entry.name == name);
    match entry.map(|entry| (entry.class, entry.variant)) {
        Some((ParallelClass::Scalar | ParallelClass::Sealed(_), _)) => Placement::Detached,
        Some((ParallelClass::World, Some(ParallelVariant::Arith(ArithOperands::Numeric)))) => {
            Placement::DetachedIfNumeric
        }
        Some((ParallelClass::World, _)) | None => Placement::Attach(AttachReason::Class),
    }
}

/// Whether the primitive NAME runs attached in a detached thread (for any
/// arguments).
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the seams resolve placements directly")
)]
pub(crate) fn needs_attach(name: &str) -> bool {
    matches!(placement(name), Placement::Attach(_))
}

/// The detached primitives, by native function address and by symbol.
/// Everything absent attaches for its class.
struct DetachedTable {
    by_function: HashMap<usize, Placement>,
    by_symbol: HashMap<SymId, Placement>,
}

static DETACHED_TABLE: OnceLock<DetachedTable> = OnceLock::new();

fn function_address(function: SubrFn) -> usize {
    match function {
        SubrFn::Many(f) => f as usize,
        SubrFn::ManyNoContext(f) => f as usize,
        SubrFn::ManySlice(f) => f as usize,
        SubrFn::A0(f) => f as usize,
        SubrFn::A1(f) => f as usize,
        SubrFn::A2(f) => f as usize,
        SubrFn::A3(f) => f as usize,
        SubrFn::A4(f) => f as usize,
        SubrFn::A5(f) => f as usize,
        SubrFn::A6(f) => f as usize,
        SubrFn::A7(f) => f as usize,
        SubrFn::A8(f) => f as usize,
    }
}

impl DetachedTable {
    /// Resolve every inventory primitive's registration. One native
    /// function behind several names runs detached only if every one of
    /// them may.
    fn build() -> Self {
        let mut by_function: HashMap<usize, Placement> = HashMap::new();
        let mut by_symbol = HashMap::new();
        let names = inventory()
            .iter()
            .filter(|entry| entry.kind == CatalogKind::Primitive)
            .map(|entry| entry.name)
            .chain(DETACHED_NUMERIC.iter().map(|row| row.0))
            .chain(DETACHED_EVALUATOR.iter().map(|row| row.0));
        for name in names {
            let placement = placement(name);
            let sym = neovm_value::intern::intern(name);
            if placement != Placement::Attach(AttachReason::Class) {
                by_symbol.insert(sym, placement);
            }
            let Some(function) = crate::emacs_core::eval::lookup_global_subr_entry(sym)
                .and_then(|entry| entry.function)
            else {
                continue;
            };
            let merged = match by_function.get(&function_address(function)) {
                None => placement,
                Some(Placement::Attach(reason)) => Placement::Attach(*reason),
                Some(_) if matches!(placement, Placement::Attach(_)) => placement,
                Some(Placement::DetachedIfNumeric) => Placement::DetachedIfNumeric,
                Some(Placement::Detached) => placement,
            };
            by_function.insert(function_address(function), merged);
        }
        by_function.retain(|_, placement| *placement != Placement::Attach(AttachReason::Class));
        Self {
            by_function,
            by_symbol,
        }
    }

    fn get() -> &'static Self {
        DETACHED_TABLE.get_or_init(Self::build)
    }
}

/// Resolve the detached table now, while this OS thread holds the Baton
/// (it reads the thread's subr registration table).
pub(crate) fn prepare_detached_table() {
    DetachedTable::get();
}

fn attach_reason(placement: Option<Placement>, args: &[Value]) -> Option<AttachReason> {
    match placement {
        Some(Placement::Detached) => None,
        Some(Placement::DetachedIfNumeric) if args.iter().all(|arg| arg.is_number()) => None,
        Some(Placement::DetachedIfNumeric) => Some(AttachReason::Class),
        Some(Placement::Attach(reason)) => Some(reason),
        None => Some(AttachReason::Class),
    }
}

/// Why calling FUNCTION on ARGS must attach, when this OS thread runs
/// detached; `None` when it may run the call as it is.
#[inline]
pub(crate) fn subr_attach_reason(function: SubrFn, args: &[Value]) -> Option<AttachReason> {
    if !neovm_value::detached::is_detached() {
        return None;
    }
    let placement = DetachedTable::get()
        .by_function
        .get(&function_address(function))
        .copied();
    attach_reason(placement, args)
}

/// [`subr_attach_reason`] for a primitive the bytecode names by symbol.
#[inline]
pub(crate) fn symbol_attach_reason(symbol: SymId, args: &[Value]) -> Option<AttachReason> {
    if !neovm_value::detached::is_detached() {
        return None;
    }
    let placement = DetachedTable::get().by_symbol.get(&symbol).copied();
    attach_reason(placement, args)
}

/// The gate's attach: take the Baton for one heap access.
///
/// # Safety
///
/// LOCK is the `BatonLock` [`detached_gate`] erased, still alive.
unsafe fn gate_attach(lock: *const (), reason: AttachReason) -> *mut () {
    // SAFETY: the caller's contract.
    let lock = unsafe { &*lock.cast::<BatonLock>() };
    lock.count_attach(reason);
    Box::into_raw(Box::new(lock.attach().into_baton())).cast()
}

/// The gate's detach: a collection the access made due waits for the
/// detached threads; then the Baton goes back.
///
/// # Safety
///
/// As [`gate_attach`]; TOKEN is what it returned.
unsafe fn gate_detach(lock: *const (), token: *mut ()) {
    // SAFETY: the caller's contract.
    let lock = unsafe { &*lock.cast::<BatonLock>() };
    // SAFETY: `gate_attach` boxed this Baton, and each token detaches once.
    let baton = unsafe { Box::from_raw(token.cast::<Baton>()) };
    neovm_value::tagged::gc::with_tagged_heap(|heap| {
        if heap.should_collect() {
            lock.defer_collection(heap.bytes_since_gc() as u64);
        }
    });
    lock.detach(*baton);
}

/// The gate's count of a detached read.
///
/// # Safety
///
/// As [`gate_attach`].
unsafe fn gate_note(lock: *const (), reason: AttachReason) {
    // SAFETY: the caller's contract.
    unsafe { &*lock.cast::<BatonLock>() }.count_attach(reason);
}

/// The gate a detached thread of LOCK's World runs behind. LOCK must
/// outlive the thread's detached body (the World holds it).
pub(crate) fn detached_gate(lock: &BatonLock) -> DetachedGate {
    DetachedGate::new(
        std::ptr::from_ref(lock).cast(),
        gate_attach,
        gate_detach,
        gate_note,
    )
}

impl Context {
    /// Run F attached when this OS thread runs detached: take the Baton
    /// for REASON, seat it for F (a wait inside F gives it up and takes it
    /// back as on any thread), and detach again after. If F unwinds, the
    /// thread stays attached; its body's end expects either.
    pub(crate) fn with_parallel_attach<R>(
        &mut self,
        reason: AttachReason,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let attach = self.parallel_attach(reason);
        let result = f(self);
        if let Some(attach) = attach {
            self.parallel_detach(attach);
        }
        result
    }

    /// The first half of [`Self::with_parallel_attach`], for a caller whose
    /// call needs more than this Context: `None` on an attached thread.
    pub(crate) fn parallel_attach(&mut self, reason: AttachReason) -> Option<ParallelAttach> {
        let lock = self.parallel_lock_shared()?;
        let gate = neovm_value::detached::suspend_detached()?;
        lock.count_attach(reason);
        self.parallel_seat_baton(lock.attach().into_baton());
        lock.body_attached();
        Some(ParallelAttach { lock, gate })
    }

    /// The second half of [`Self::with_parallel_attach`].
    pub(crate) fn parallel_detach(&mut self, attach: ParallelAttach) {
        let baton = self.parallel_unseat_baton();
        attach.lock.body_detached();
        attach.lock.detach(baton);
        neovm_value::detached::resume_detached(attach.gate);
    }

    /// Whether a GC safe point must not collect now: a detached thread
    /// never does, and while any thread runs detached a due collection is
    /// deferred (slice S3's stage-2 limitation: their stacks hold roots no
    /// collector can see until slice S4).
    #[inline]
    pub(crate) fn parallel_defers_safe_point(&self) -> bool {
        if neovm_value::detached::is_detached() {
            return true;
        }
        let Some(lock) = self.parallel_lock() else {
            return false;
        };
        if Self::detached_peers(lock) == 0 {
            return false;
        }
        if self.tagged_heap.should_collect() {
            lock.defer_collection(self.tagged_heap.bytes_since_gc() as u64);
        }
        true
    }

    /// [`Self::parallel_defers_safe_point`] at a collection's entry; a
    /// collection that runs after a deferral is counted.
    pub(crate) fn parallel_defers_collection(&self) -> bool {
        if neovm_value::detached::is_detached() {
            return true;
        }
        let Some(lock) = self.parallel_lock() else {
            return false;
        };
        if Self::detached_peers(lock) > 0 {
            lock.defer_collection(self.tagged_heap.bytes_since_gc() as u64);
            return true;
        }
        lock.collection_runs();
        false
    }

    /// Bodies running detached now. This OS thread, attached, is not one:
    /// a body that seats the Baton leaves the count, and a peer that then
    /// parks in a wait has published its roots in the parked registry.
    fn detached_peers(lock: &BatonLock) -> u64 {
        lock.detached_threads()
    }

    /// A detached thread binds SYM dynamically. A projected variable binds
    /// in the thread's owner slab and stays detached; any other binding is
    /// the shared cell's, which every thread reads, so the thread attaches
    /// and stays attached until the binding unwinds
    /// ([`Self::parallel_unpin_after_unbind`]), as GNU's thread holds the
    /// global lock across it.
    pub(crate) fn parallel_pin_for_binding(&mut self, sym: SymId) {
        if !neovm_value::detached::is_detached() || self.binds_in_owner_slab(sym) {
            return;
        }
        let Some(lock) = self.parallel_lock_shared() else {
            return;
        };
        if neovm_value::detached::pin_at(self.specpdl.len()) {
            lock.count_attach(AttachReason::Class);
            self.parallel_seat_baton(lock.attach().into_baton());
            lock.body_attached();
        }
    }

    /// The bindings unwound: a thread pinned below them detaches again.
    pub(crate) fn parallel_unpin_after_unbind(&mut self) {
        if !neovm_value::detached::running_detached() || neovm_value::detached::is_detached() {
            return;
        }
        let Some(gate) = neovm_value::detached::unpin_at(self.specpdl.len()) else {
            return;
        };
        let Some(lock) = self.parallel_lock_shared() else {
            return;
        };
        let baton = self.parallel_unseat_baton();
        lock.body_detached();
        lock.detach(baton);
        neovm_value::detached::resume_detached(gate);
    }

    /// Whether a dynamic binding of SYM lands in this thread's owner slab.
    fn binds_in_owner_slab(&self, sym: SymId) -> bool {
        matches!(
            self.obarray.get_by_id(sym).map(|symbol| symbol.value_cell()),
            Some(neovm_value::symbol::ValueCell::Forwarded(fwd))
                if fwd.thread_projection().is_some()
        )
    }

    /// GNU `thread-signal` reaches a detached thread through its quit
    /// request: at its next quit poll the thread raises the signal stored
    /// for it, attached. Any other quit request is left for the ordinary
    /// poll.
    pub(crate) fn parallel_raise_thread_signal(&mut self) -> Result<(), Flow> {
        if self.parallel_lock().is_none()
            || !neovm_value::detached::running_detached()
            || !self.quit_requested.is_requested()
        {
            return Ok(());
        }
        let Some(key) = self.green_key() else {
            return Ok(());
        };
        self.with_parallel_attach(AttachReason::Class, |ctx| {
            if !ctx.threads.has_pending_thread_signal(key.id()) {
                return Ok(());
            }
            ctx.quit_requested.clear();
            ctx.green_raise_pending_signal(key)
        })
    }
}
