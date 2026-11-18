use super::*;
use crate::value::Value;

#[test]
fn repeated_reads_skip_capture_state_but_preserve_nested_dependencies() {
    let source = Value::cons(Value::NIL, Value::NIL);
    let (_, outer) = capture(|| {
        OBSERVATION_STATE_ACCESSES.with(|count| count.set(0));
        for _ in 0..1024 {
            source.cons_car();
        }
        assert_eq!(OBSERVATION_STATE_ACCESSES.with(Cell::get), 1);
        let (_, inner) = capture(|| {
            for _ in 0..1024 {
                source.cons_cdr();
            }
        });
        assert_eq!(OBSERVATION_STATE_ACCESSES.with(Cell::get), 2);
        source.set_car(Value::T);
        assert!(!inner.unwrap().unchanged());
        source.cons_car();
        assert_eq!(OBSERVATION_STATE_ACCESSES.with(Cell::get), 3);
    });
    assert!(
        outer.is_none(),
        "the first read must still precede the mutation"
    );
}

#[test]
fn observed_cons_survives_unrelated_mutation_but_not_its_own() {
    let source = Value::list(vec![Value::fixnum(1), Value::fixnum(2)]);
    let unrelated = Value::cons(Value::NIL, Value::NIL);
    let (_, reads) = capture(|| source.cons_cdr().cons_car());
    let reads = reads.unwrap();
    unrelated.set_cdr(Value::T);
    assert!(reads.unchanged());
    source.cons_cdr().set_car(Value::fixnum(3));
    assert!(!reads.unchanged());
}

#[test]
fn nested_capture_and_write_during_observation_are_conservative() {
    let source = Value::cons(Value::NIL, Value::NIL);
    let (_, outer) = capture(|| {
        let (_, inner) = capture(|| source.cons_car());
        assert!(inner.unwrap().unchanged());
    });
    let outer = outer.unwrap();
    source.set_car(Value::T);
    assert!(!outer.unchanged());
    let (_, invalid) = capture(|| {
        source.cons_car();
        source.set_car(Value::NIL);
    });
    assert!(invalid.is_none());
    let (_, fresh) = capture(|| {
        source.set_car(Value::T);
        source.cons_car()
    });
    assert!(fresh.unwrap().unchanged());
}

#[test]
fn nested_cache_hit_propagates_dependencies() {
    let source = Value::cons(Value::NIL, Value::NIL);
    let (_, inner) = capture(|| source.cons_car());
    let inner = inner.unwrap();
    let (_, outer) = capture(|| assert!(inner.unchanged_and_observe()));
    source.set_car(Value::T);
    assert!(!outer.unwrap().unchanged());
}

#[test]
fn normalization_retains_dependencies_without_rebasing_outer_reads() {
    let mut source = Value::cons(Value::NIL, Value::NIL);
    let (_, outer) = capture(|| {
        source.cons_car();
        let (result, inner) = capture_normalized(
            &mut source,
            |source| {
                source.cons_car();
                source.set_car(Value::T);
            },
            |source| source.cons_car(),
        );
        assert_eq!(result, Value::T);
        assert!(inner.unwrap().unchanged());
    });
    assert!(outer.is_none());
    let (_, reads) = capture_normalized(
        &mut source,
        |source| {
            source.cons_car();
            source.set_car(Value::NIL);
        },
        |_| (),
    );
    source.set_car(Value::T);
    assert!(!reads.unwrap().unchanged());
}

#[test]
fn nested_scope_budget_is_bounded_and_recovers() {
    fn nested(depth: usize) {
        let (_, reads) = capture(|| {
            assert!(with_state(|s| s.borrow().captures.len()) <= MAX_DEPTH);
            if depth > 0 {
                nested(depth - 1);
            }
        });
        assert!(reads.is_none());
    }
    nested(MAX_DEPTH + 5);
    assert!(!ACTIVE.with(Cell::get));
    assert!(capture(|| ()).1.unwrap().unchanged());
}

#[test]
fn vector_string_and_character_table_mutations_invalidate_reads() {
    let vector = Value::vector(vec![Value::NIL]);
    let string = Value::string("abc");
    let table = Value::make_char_table(Value::NIL, Value::NIL, 0);
    let (_, reads) = capture(|| vector.as_vector_data().unwrap().len());
    vector.set_vector_slot(0, Value::T);
    assert!(!reads.unwrap().unchanged());
    let (_, reads) = capture(|| string.as_str_owned());
    string.set_string_byte_same_char_count(0, b'z');
    assert!(!reads.unwrap().unchanged());
    let (_, reads) = capture(|| table.as_char_table_obj().unwrap().defalt.get());
    table.with_char_table_mut(|table| table.set_default(Value::T));
    assert!(!reads.unwrap().unchanged());
}

#[test]
fn lost_mutation_history_refuses_reuse() {
    let source = Value::cons(Value::NIL, Value::NIL);
    let unrelated = Value::cons(Value::NIL, Value::NIL);
    let (_, reads) = capture(|| source.cons_car());
    let reads = reads.unwrap();
    for _ in 0..=JOURNAL_SIZE {
        unrelated.set_car(Value::T);
    }
    assert!(!reads.unchanged());
}

#[test]
fn observation_budget_and_unwind_restore_the_outer_scope() {
    let values: Vec<_> = (0..=MAX_READS)
        .map(|_| Value::cons(Value::NIL, Value::NIL))
        .collect();
    let (_, reads) = capture(|| {
        for value in &values {
            value.cons_car();
        }
    });
    assert!(reads.is_none());
    let _ = std::panic::catch_unwind(|| capture(|| panic!("unwind")));
    assert!(!ACTIVE.with(Cell::get));
    let (_, reads) = capture(|| values[0].cons_car());
    assert!(reads.unwrap().unchanged());
}

#[cfg(test)]
#[path = "fast_paths_test.rs"]
mod fast_paths;

/// A thread running detached stores without a revision or a ring slot
/// (feature `shared-heap`): the threads would otherwise share that line on
/// every store. Its first store refuses every certificate until a stopped
/// World ends the extent, and one begun before the end stays refused.
#[cfg(feature = "shared-heap")]
#[test]
fn detached_stores_take_no_revision_and_refuse_every_certificate() {
    use crate::detached::{AttachReason, DetachedGate, begin_detached, end_detached};
    unsafe fn attach(_lock: *const (), _reason: AttachReason) -> *mut () {
        panic!("a store does not attach");
    }
    unsafe fn detach(_lock: *const (), _token: *mut ()) {}
    unsafe fn note(_lock: *const (), _reason: AttachReason) {}
    unsafe fn defer(_lock: *const (), _bytes: u64) {}
    let source = Value::cons(Value::NIL, Value::NIL);
    let unrelated = Value::cons(Value::NIL, Value::NIL);
    let (_, reads) = capture(|| source.cons_car());
    let reads = reads.unwrap();
    assert!(reads.unchanged());

    let before = LispCollectionRevision::current();
    begin_detached(DetachedGate::new(
        std::ptr::null(),
        attach,
        detach,
        note,
        defer,
    ));
    for _ in 0..1000 {
        unrelated.set_car(Value::T);
    }
    let after = LispCollectionRevision::current();
    end_detached();
    assert_eq!(
        after.steps_since_for_test(before),
        0,
        "1000 detached stores"
    );
    assert!(
        !reads.unchanged(),
        "the journal missed the detached stores, so nothing is reused"
    );

    with_store(|store| store.journal_detached_extent());
    assert!(
        !reads.unchanged(),
        "a certificate begun inside the extent stays refused after it"
    );
    let (_, fresh) = capture(|| source.cons_car());
    let fresh = fresh.unwrap();
    unrelated.set_car(Value::NIL);
    assert!(fresh.unchanged(), "the Baton holder journals again");
    source.set_car(Value::T);
    assert!(!fresh.unchanged());
}
