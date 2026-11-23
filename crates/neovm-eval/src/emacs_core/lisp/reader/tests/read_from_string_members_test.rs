use super::*;

fn read(ctx: &mut crate::emacs_core::eval::Context, text: &str) -> Value {
    builtin_read_from_string(ctx, vec![Value::string(text)])
        .expect("read-from-string succeeds")
        .cons_car()
}

fn is_member(ctx: &crate::emacs_core::eval::Context, name: &str) -> bool {
    ctx.obarray().intern_soft(name).is_some()
}

#[test]
fn small_form_symbols_become_obarray_members() {
    crate::test_utils::init_test_tracing();
    let mut ctx = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    let form = read(
        &mut ctx,
        "(rfs-small-a (rfs-small-b . \"s\") 1.5 :rfs-small-kw)",
    );
    assert!(form.is_cons());
    for name in ["rfs-small-a", "rfs-small-b", ":rfs-small-kw"] {
        assert!(is_member(&ctx, name), "{name} is a member");
    }
}

#[test]
fn form_past_the_small_budget_still_interns_its_last_symbol() {
    crate::test_utils::init_test_tracing();
    let mut ctx = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    let text = format!(
        "({} rfs-long-tail)",
        "1 ".repeat(MemberWalkVisited::UNHASHED_BUDGET + 10)
    );
    read(&mut ctx, &text);
    assert!(is_member(&ctx, "rfs-long-tail"));
}

#[test]
fn vector_element_symbols_use_the_full_walk() {
    crate::test_utils::init_test_tracing();
    let mut ctx = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    read(&mut ctx, "(rfs-vec-head [rfs-vec-elt])");
    assert!(is_member(&ctx, "rfs-vec-head"));
    assert!(is_member(&ctx, "rfs-vec-elt"));
}

#[test]
fn circular_form_terminates_and_interns() {
    crate::test_utils::init_test_tracing();
    let mut ctx = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    read(&mut ctx, "#1=(rfs-cycle . #1#)");
    assert!(is_member(&ctx, "rfs-cycle"));
}
