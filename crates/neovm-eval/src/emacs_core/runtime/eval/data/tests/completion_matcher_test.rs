//! Private matcher tests move with their single completion implementation.

use super::*;

#[test]
fn completion_prefix_models_empty_input_as_match_all() {
    let empty = LispString::from_utf8("");
    let nonempty = LispString::from_utf8("M-x");

    assert_eq!(
        CompletionPrefix::from_lisp_string(&empty),
        CompletionPrefix::Empty
    );
    assert_eq!(
        CompletionPrefix::from_lisp_string(&nonempty),
        CompletionPrefix::Characters(vec!['M' as u32, '-' as u32, 'x' as u32])
    );
}

#[test]
fn completion_candidate_keeps_the_global_obarray_scan_compact() {
    assert!(
        std::mem::size_of::<CompletionCandidate>() <= 5 * std::mem::size_of::<usize>(),
        "completion candidates should carry compact ownership handles, not inline LispString payloads; got {} bytes",
        std::mem::size_of::<CompletionCandidate>()
    );
}

/// Candidates tested against `completion-regexp-list` while FORM evaluates,
/// and its printed value.
fn regexp_tests_during(
    context: &mut crate::emacs_core::eval::Context,
    form: &str,
) -> (usize, String) {
    let before = COMPLETION_REGEXP_TESTS.with(std::cell::Cell::get);
    let value = context.eval_str(form).expect("form evaluates");
    let tested = COMPLETION_REGEXP_TESTS.with(std::cell::Cell::get) - before;
    (tested, crate::emacs_core::print::print_value(&value))
}

fn narrowing_context() -> crate::emacs_core::eval::Context {
    let mut context = crate::emacs_core::eval::Context::new(crate::test_utils::unit_harness());
    context
        .eval_str(
            r#"(setq narrowing-candidates
                     (list "buffer-substring" "Buffer-Name" "bufferp" "insert-buffer-substring"
                           "find-file" "font-lock-face" "buffer-string" 'buffer-sym))"#,
        )
        .expect("candidates");
    context
}

#[test]
fn all_completions_tests_only_survivors_when_the_literal_input_extends() {
    let mut context = narrowing_context();
    let (first, result) = regexp_tests_during(
        &mut context,
        r#"(let ((completion-regexp-list '("buf"))) (all-completions "" narrowing-candidates))"#,
    );
    assert_eq!(first, 8, "the first call tests every candidate");
    assert_eq!(
        result,
        r#"("buffer-substring" "bufferp" "insert-buffer-substring" "buffer-string" "buffer-sym")"#
    );
    // One more character, then a second literal component.
    let (second, result) = regexp_tests_during(
        &mut context,
        r#"(let ((completion-regexp-list '("buff"))) (all-completions "" narrowing-candidates))"#,
    );
    assert_eq!(second, 5, "only the previous call's survivors are tested");
    assert_eq!(
        result,
        r#"("buffer-substring" "bufferp" "insert-buffer-substring" "buffer-string" "buffer-sym")"#
    );
    let (third, result) = regexp_tests_during(
        &mut context,
        r#"(let ((completion-regexp-list '("buff" "str"))) (all-completions "" narrowing-candidates))"#,
    );
    assert_eq!(third, 5);
    assert_eq!(
        result,
        r#"("buffer-substring" "insert-buffer-substring" "buffer-string")"#
    );
    // Deleting a character does not narrow: every candidate is tested again.
    let (widened, _) = regexp_tests_during(
        &mut context,
        r#"(let ((completion-regexp-list '("buf"))) (all-completions "" narrowing-candidates))"#,
    );
    assert_eq!(widened, 8);
}

#[test]
fn all_completions_narrowing_sees_candidates_changed_between_calls() {
    let mut context = narrowing_context();
    regexp_tests_during(
        &mut context,
        r#"(let ((completion-regexp-list '("buf"))) (all-completions "" narrowing-candidates))"#,
    );
    // "find-file" becomes "bufd-file" in place: the texts differ, so the
    // extended input is tested against every candidate.
    let (tested, result) = regexp_tests_during(
        &mut context,
        r#"(progn (aset (nth 4 narrowing-candidates) 0 ?b)
                  (aset (nth 4 narrowing-candidates) 1 ?u)
                  (aset (nth 4 narrowing-candidates) 2 ?f)
                  (let ((completion-regexp-list '("bufd")))
                    (all-completions "" narrowing-candidates)))"#,
    );
    assert_eq!(tested, 8);
    assert_eq!(result, r#"("bufd-file")"#);
    // A different case folding is a different key.
    let (tested, result) = regexp_tests_during(
        &mut context,
        r#"(let ((completion-ignore-case t) (completion-regexp-list '("bufd-")))
             (all-completions "" narrowing-candidates))"#,
    );
    assert_eq!(tested, 8);
    assert_eq!(result, r#"("bufd-file")"#);
}

#[test]
fn all_completions_does_not_narrow_past_a_predicate_or_a_non_literal_regexp() {
    let mut context = narrowing_context();
    for (earlier, later) in [
        (
            r#"(let ((completion-regexp-list '("buf"))) (all-completions "" narrowing-candidates #'stringp))"#,
            r#"(let ((completion-regexp-list '("buff"))) (all-completions "" narrowing-candidates #'stringp))"#,
        ),
        (
            r#"(let ((completion-regexp-list '("^buf"))) (all-completions "" narrowing-candidates))"#,
            r#"(let ((completion-regexp-list '("^buff"))) (all-completions "" narrowing-candidates))"#,
        ),
    ] {
        regexp_tests_during(&mut context, earlier);
        let (tested, _) = regexp_tests_during(&mut context, later);
        assert_eq!(tested, 8, "{later}");
    }
}

#[test]
fn completion_narrowing_key_requires_extension_and_the_same_folding() {
    let context = narrowing_context();
    let syntax =
        crate::emacs_core::builtins::search::FastStringMatchSyntax::for_current_buffer(&context);
    let key = |prefix: &str, regexps: &[&str], ignore_case: bool| {
        let regexps: Vec<LispString> = regexps.iter().map(|r| LispString::from_utf8(r)).collect();
        let prepared = CompletionRegexps::new(syntax, &regexps, ignore_case);
        CompletionNarrowingKey::new(&LispString::from_utf8(prefix), &prepared, ignore_case)
    };
    let earlier = key("", &["buf", "str"], false).expect("literal regexps key a call");
    assert!(key("", &["buff", "str"], false).unwrap().narrows(&earlier));
    assert!(
        key("", &["xstr", "buffer"], false)
            .unwrap()
            .narrows(&earlier)
    );
    assert!(
        key("", &["buf", "str", "z"], false)
            .unwrap()
            .narrows(&earlier)
    );
    assert!(!key("", &["buf"], false).unwrap().narrows(&earlier));
    assert!(!key("", &["buff", "st"], false).unwrap().narrows(&earlier));
    assert!(!key("", &["buff", "str"], true).unwrap().narrows(&earlier));
    let prefixed = key("b", &["buf"], false).unwrap();
    assert!(key("bu", &["buf"], false).unwrap().narrows(&prefixed));
    assert!(!key("", &["buf"], false).unwrap().narrows(&prefixed));
    assert!(!key("c", &["buf"], false).unwrap().narrows(&prefixed));
    // A regexp that is not one literal run, or a case-ignoring non-empty
    // prefix (the case table may change), keys nothing.
    assert!(key("", &["buf", "^str"], false).is_none());
    assert!(key("", &["b\\(u\\)f"], false).is_none());
    assert!(key("b", &["buf"], true).is_none());
    assert!(key("", &["buf"], true).is_some());
    // Folded literals compare folded.
    let folded = key("", &["BUF"], true).unwrap();
    assert!(key("", &["buffer"], true).unwrap().narrows(&folded));
}
