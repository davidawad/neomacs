use super::*;
use neovm_value::intern::intern;
use neovm_value::symbol::Obarray;

fn fresh_symbol(name: &str) -> Value {
    Value::from_sym_id(intern(name))
}

fn is_member(obarray: &Obarray, name: &str) -> bool {
    obarray.intern_soft(name).is_some()
}

#[test]
fn a_small_tree_is_published_without_an_identity_set() {
    let _harness = crate::test_utils::unit_harness();
    let mut obarray = Obarray::new();
    let tree = Value::list(vec![
        fresh_symbol("member-walk-small-a"),
        Value::list(vec![fresh_symbol("member-walk-small-b")]),
        Value::vector(vec![fresh_symbol("member-walk-small-c")]),
    ]);
    obarray.intern_member_symbols_in(tree);
    for name in ["a", "b", "c"] {
        assert!(
            is_member(&obarray, &format!("member-walk-small-{name}")),
            "{name}"
        );
    }
}

#[test]
fn a_form_larger_than_the_unhashed_budget_is_published_to_its_last_symbol() {
    let _harness = crate::test_utils::unit_harness();
    let mut obarray = Obarray::new();
    let count = MemberWalkVisited::UNHASHED_BUDGET + 904;
    let items = (0..count)
        .map(|i| fresh_symbol(&format!("member-walk-big-{i}")))
        .collect::<Vec<_>>();
    obarray.intern_member_symbols_in(Value::list(items));
    assert!(is_member(&obarray, "member-walk-big-0"));
    assert!(is_member(
        &obarray,
        &format!("member-walk-big-{}", count - 1)
    ));
}

#[test]
fn a_cycle_terminates_and_publishes_what_it_reaches() {
    let _harness = crate::test_utils::unit_harness();
    let mut obarray = Obarray::new();
    let head = Value::cons(fresh_symbol("member-walk-cycle-a"), Value::NIL);
    let tail = Value::cons(fresh_symbol("member-walk-cycle-b"), head);
    head.set_cdr(tail);
    obarray.intern_member_symbols_in(head);
    assert!(is_member(&obarray, "member-walk-cycle-a"));
    assert!(is_member(&obarray, "member-walk-cycle-b"));
}

#[test]
fn a_form_that_shares_its_subtrees_is_not_walked_once_per_path() {
    let _harness = crate::test_utils::unit_harness();
    let mut obarray = Obarray::new();
    // 2^60 paths to the leaf if each were walked.
    let mut node = Value::cons(fresh_symbol("member-walk-shared-leaf"), Value::NIL);
    for _ in 0..60 {
        node = Value::cons(node, node);
    }
    obarray.intern_member_symbols_in(node);
    assert!(is_member(&obarray, "member-walk-shared-leaf"));
}
