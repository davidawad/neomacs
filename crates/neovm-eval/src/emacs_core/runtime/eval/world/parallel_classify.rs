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

use crate::emacs_core::eval::subr_abi::SubrFn;
use crate::emacs_core::subr::effect_catalog::inventory;
use crate::emacs_core::subr::effect_schema::{
    ArithOperands, CatalogKind, ParallelClass, ParallelVariant,
};
use neovm_value::detached::AttachReason;
use neovm_value::intern::SymId;
use neovm_value::value::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

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
