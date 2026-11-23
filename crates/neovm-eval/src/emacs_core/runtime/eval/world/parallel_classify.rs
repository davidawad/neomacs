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
//! collections stop detached threads at their exact safe points
//! (`world::parallel_gc`).

use super::{Baton, BatonLock, Context};
use crate::emacs_core::eval::subr_abi::SubrFn;
use crate::emacs_core::subr::effect_catalog::inventory;
use crate::emacs_core::subr::effect_schema::{
    ArithOperands, CatalogKind, ParallelClass, ParallelVariant,
};
use neovm_value::detached::{AttachReason, DetachedGate};
use neovm_value::error::Flow;
use neovm_value::intern::SymId;
use neovm_value::quit::{ASYNC_ATTENTION, AsyncSource};
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

/// Primitives the inventory leaves `World` by default that only read their
/// arguments and build fresh objects: they run detached, allocating from
/// the thread's own runs (`neovm_value::detached`); a step that needs the
/// World attaches for itself. Each row: name, why.
const DETACHED_CONSTRUCTORS: &[(&str, &str)] = &[(
    "json-parse-string",
    "reads its string argument and builds conses, strings, numbers, vectors \
     and hash tables; interning an alist or plist key attaches",
)];

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
    if DETACHED_CONSTRUCTORS.iter().any(|row| row.0 == name) {
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
            .chain(DETACHED_EVALUATOR.iter().map(|row| row.0))
            .chain(DETACHED_CONSTRUCTORS.iter().map(|row| row.0));
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

/// The gate's detach: a collection the access made due is requested at
/// the thread's next exact safe point; then the Baton goes back.
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

/// The gate's note of a collection the thread's own allocation runs made
/// due (`neovm_value::detached`), requested as one an attached access makes
/// due is ([`gate_detach`]).
///
/// # Safety
///
/// As [`gate_attach`].
unsafe fn gate_defer(lock: *const (), bytes: u64) {
    // SAFETY: the caller's contract.
    unsafe { &*lock.cast::<BatonLock>() }.defer_collection(bytes);
}

/// The gate a detached thread of LOCK's World runs behind. LOCK must
/// outlive the thread's detached body (the World holds it).
pub(crate) fn detached_gate(lock: &BatonLock) -> DetachedGate {
    DetachedGate::new(
        std::ptr::from_ref(lock).cast(),
        gate_attach,
        gate_detach,
        gate_note,
        gate_defer,
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
        // Attached for World work, the thread still runs: a stop waits for
        // it to park at a safe point.
        self.parallel_seat_baton(lock.attach().into_baton());
        Some(ParallelAttach { lock, gate })
    }

    /// The second half of [`Self::with_parallel_attach`].
    pub(crate) fn parallel_detach(&mut self, attach: ParallelAttach) {
        self.parallel_defer_due_collection(&attach.lock);
        let baton = self.parallel_unseat_baton();
        attach.lock.detach(baton);
        neovm_value::detached::resume_detached(attach.gate);
    }

    /// This thread, attached for World work, is about to run detached
    /// again. What it allocated attached may have made a collection due,
    /// and it passes no attached safe point that would see it: leave the
    /// collection due for the next detached safe point, as the allocation
    /// gate's detach does. With the main owner parked in a wait, no other
    /// thread would ever collect.
    fn parallel_defer_due_collection(&self, lock: &BatonLock) {
        let bytes = self.tagged_heap.bytes_since_gc();
        if bytes >= self.tagged_heap.gc_threshold() {
            lock.defer_collection(bytes as u64);
        }
    }

    /// A GC safe point's answer in a Parallel World when it is not the
    /// ordinary one. A detached thread reads no heap state here: it goes to
    /// the collection entry ([`Self::parallel_defers_collection`]) when a
    /// stop is requested, to park, or when its World has a collection due,
    /// to request it. An attached thread joins a requested stop.
    #[inline]
    pub(crate) fn parallel_safe_point(&self) -> Option<bool> {
        let stop_requested = ASYNC_ATTENTION.is_raised(AsyncSource::StopRequest);
        if neovm_value::detached::is_detached() {
            let due = self.gc_inhibit_depth == 0
                && self.parallel_lock().is_some_and(BatonLock::collection_due);
            return Some(stop_requested || due);
        }
        (stop_requested && self.gc_inhibit_depth == 0 && self.parallel_lock().is_some())
            .then_some(true)
    }

    /// At a collection's entry: a detached thread does not collect itself,
    /// it parks for a stop, or requests the due collection
    /// ([`Self::parallel_detached_collect`]).
    pub(crate) fn parallel_defers_collection(&mut self, force_complete: bool) -> bool {
        if neovm_value::detached::is_detached() {
            self.parallel_detached_collect(force_complete);
            return true;
        }
        false
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
            if self.project_plain_dynamic_binding(sym) {
                // A newly encountered plain variable can be made owner-local
                // while the Baton protects the obarray redirect. Future
                // reads and bindings then use this owner's slab and the
                // worker can stay detached for the binding's whole extent.
                self.parallel_unpin_after_unbind();
            }
        }
    }

    /// Project a bindable plain cell the first time a detached worker binds
    /// it. Symbols excluded from admission, such as constants, keep the
    /// attached shared-cell behavior.
    fn project_plain_dynamic_binding(&mut self, sym: SymId) -> bool {
        if let Some(symbol) = self.obarray.get_by_id(sym) {
            if symbol.trapped_write() == neovm_value::symbol::SymbolTrappedWrite::NoWrite
                || !matches!(
                    symbol.value_cell(),
                    neovm_value::symbol::ValueCell::Plain(_)
                )
            {
                return false;
            }
        }

        if self.reserve_thread_projections(1).is_err() {
            return false;
        }
        self.project_admitted_cells(&[(sym, neovm_value::symbol::AdmissionPlanCell::Owned)]);
        true
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
        self.parallel_defer_due_collection(&lock);
        let baton = self.parallel_unseat_baton();
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
