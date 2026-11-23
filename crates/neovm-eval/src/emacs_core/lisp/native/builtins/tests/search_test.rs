use super::*;

/// GNU `string_match_1` reads predeclared C variables such as
/// `Vcase_fold_search` and `Vinhibit_changing_match_data`; it does not resolve
/// their names for each match. Completion filtering calls `string-match`
/// repeatedly, so the steady-state path must likewise use canonical IDs.
#[test]
fn string_match_uses_predeclared_search_state_symbols() {
    crate::test_utils::init_test_tracing();
    let mut eval = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    let args = [Value::string("a"), Value::string("a")];

    eval.obarray_mut()
        .set_symbol_value("case-fold-search", Value::T);
    eval.obarray_mut()
        .set_symbol_value("inhibit-changing-match-data", Value::NIL);
    eval.obarray_mut()
        .set_symbol_value("char-script-table", Value::NIL);
    eval.obarray_mut()
        .set_symbol_value("word-combining-categories", Value::NIL);
    eval.obarray_mut()
        .set_symbol_value("word-separating-categories", Value::NIL);

    builtin_string_match_slice(&mut eval, &args).expect("warm string-match state");

    neovm_value::intern::reset_intern_calls();
    let result = builtin_string_match_slice(&mut eval, &args).expect("string-match");

    assert_eq!(result, Value::fixnum(0));
    assert_eq!(
        neovm_value::intern::intern_calls(),
        0,
        "steady-state string-match must use predeclared search-state symbols",
    );
}
/// A search reads the current buffer's case canon table on every call, and a
/// complete table is answered without re-deriving it.  What a caller observes
/// must still follow every table change: `set-case-table` in the buffer, a
/// buffer-local table in another buffer, and a table that has no derived
/// subsidiaries yet.
#[test]
fn string_match_follows_the_current_buffers_case_table() {
    use crate::emacs_core::eval::case_tables::current_case_canon_table;
    crate::test_utils::init_test_tracing();
    let mut eval = crate::emacs_core::eval::Context::new(crate::test_utils::unit_runtime());
    let run = |eval: &mut crate::emacs_core::eval::Context, form: &str| {
        crate::emacs_core::format_eval_result(&eval.eval_str(form))
    };

    assert_eq!(
        run(
            &mut eval,
            r#"(list (string-match "A" "a") (string-match "b" "B"))"#
        ),
        "OK (0 0)"
    );
    // The steady state: the canon table is the same object call after call.
    let standard = current_case_canon_table(&mut eval).expect("standard canon table");
    assert!(!standard.is_nil());
    assert_eq!(
        current_case_canon_table(&mut eval).expect("again").bits(),
        standard.bits()
    );

    // A bare case table has no up/canon/eqv yet: the search derives them and
    // folds nothing, then the standard table folds again.
    assert_eq!(
        run(
            &mut eval,
            r#"(let ((std (current-case-table)) (bare (make-char-table 'case-table)) (in-custom nil))
                 (set-case-table bare)
                 (setq in-custom (list (string-match "A" "a") (string-match "b" "B")
                                       (string-match "a" "a")))
                 (set-case-table std)
                 (list in-custom (string-match "A" "a")))"#
        ),
        "OK ((nil nil 0) 0)"
    );
    let custom = run(
        &mut eval,
        r#"(let ((bare (make-char-table 'case-table)))
             (set-case-table bare)
             (setq saved-canon-probe (list (string-match "A" "a")))
             (let ((outer-buffer (current-buffer)))
               (set-buffer (get-buffer-create "other"))
               (setq other-probe (string-match "A" "a"))
               (set-buffer outer-buffer))
             (list saved-canon-probe other-probe (string-match "A" "a")))"#,
    );
    assert_eq!(custom, "OK ((nil) 0 nil)", "case tables are per buffer");
    let after = current_case_canon_table(&mut eval).expect("canon after switch");
    assert_ne!(after.bits(), standard.bits(), "the buffer's own table wins");
}
