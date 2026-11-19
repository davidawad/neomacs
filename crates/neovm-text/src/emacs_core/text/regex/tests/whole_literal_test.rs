//! A pattern that is one literal run is searched without the matcher, and
//! finds exactly what the matcher finds.

use super::*;

type CapturedMatch = Option<(usize, Vec<i64>, Vec<i64>)>;

fn capture(found: Option<(usize, MatchRegisters)>) -> CapturedMatch {
    found.map(|(position, regs)| (position, regs.start.to_vec(), regs.end.to_vec()))
}

/// The search as `re_search` runs it, and the number of matcher entries.
fn optimized(
    pattern: &CompiledPattern,
    text: &[u8],
    start: usize,
    range: isize,
) -> (CapturedMatch, u64) {
    let before = matcher_entry_count();
    // The DFA off, so every candidate the general path tries is counted.
    let found = dfa::with_dfa_mode(dfa::DfaMode::Off, || {
        re_search(pattern, text, start, range, &DefaultSyntaxLookup, start)
    });
    (capture(found), matcher_entry_count() - before)
}

/// The oracle: every search skip off, so every candidate enters the matcher.
fn matcher_only(
    pattern: &CompiledPattern,
    text: &[u8],
    start: usize,
    range: isize,
) -> CapturedMatch {
    with_fastmap_disabled(|| {
        capture(re_search(
            pattern,
            text,
            start,
            range,
            &DefaultSyntaxLookup,
            start,
        ))
    })
}

fn compile(expression: &str, case_fold: bool, target_multibyte: bool) -> CompiledPattern {
    let mut pattern = regex_compile(expression, false, case_fold).expect("valid pattern");
    pattern.target_multibyte = target_multibyte;
    pattern
}

#[test]
fn whole_literal_is_derived_only_for_a_bare_literal_run() {
    crate::test_utils::init_test_tracing();
    for expression in ["buffer-sub", "a\\.b\\*c", "\\^def", "ab$c", "Жq", "x\\?y"] {
        assert!(
            compile(expression, false, true).whole_literal.is_some(),
            "{expression:?} is one literal run"
        );
    }
    for expression in [
        "", "^def", "def$", "a.b", "ab*", "\\(ab\\)", "ab\\|cd", "\\bab", "\\`ab", "[ab]",
        "a\\{2\\}",
    ] {
        assert!(
            compile(expression, false, true).whole_literal.is_none(),
            "{expression:?} is not one literal run"
        );
    }
}

#[test]
fn whole_literal_search_skips_the_matcher() {
    crate::test_utils::init_test_tracing();
    let text = "find-file-other-window-buffer-substring-no-properties";
    for case_fold in [false, true] {
        let pattern = compile("buffer-sub", case_fold, true);
        let (found, entries) = optimized(&pattern, text.as_bytes(), 0, text.len() as isize);
        assert_eq!(
            found,
            matcher_only(&pattern, text.as_bytes(), 0, text.len() as isize)
        );
        assert_eq!(
            found,
            Some((23, vec![23], vec![33])),
            "case_fold={case_fold}"
        );
        assert_eq!(
            entries, 0,
            "a whole literal needs no matcher entry (case_fold={case_fold})"
        );
    }
}

#[test]
fn whole_literal_search_agrees_with_the_matcher_at_every_bound() {
    crate::test_utils::init_test_tracing();
    let texts = [
        "",
        "abc",
        "xxABCabcxx",
        "ЖqжQ",
        "жЖжq!",
        "xΣqςQσq",
        "Жq\nжQ",
        "éЖq😀Жq",
        "Kelvin \u{212A}elvin kELVIN",
        "aaab aab ab",
    ];
    let expressions = [
        "abc", "ABC", "b", "жq", "Жq", "σq", "ж", "éЖq", "q\\!", "kelvin", "aab", "ab ", "\\[x",
    ];
    for text in texts {
        // Every byte bound, including ones inside encoded characters: no
        // match may start inside a character or run past the bound.
        let positions: Vec<_> = (0..=text.len()).collect();
        for expression in expressions {
            for case_fold in [false, true] {
                let pattern = compile(expression, case_fold, true);
                for &start in &positions {
                    for &stop in &positions {
                        let range = stop as isize - start as isize;
                        assert_eq!(
                            optimized(&pattern, text.as_bytes(), start, range).0,
                            matcher_only(&pattern, text.as_bytes(), start, range),
                            "expression={expression:?} case_fold={case_fold} \
                             text={text:?} start={start} stop={stop}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn whole_literal_search_agrees_with_the_matcher_on_unibyte_text() {
    crate::test_utils::init_test_tracing();
    // Raw bytes, Latin-1 bytes and ASCII in a unibyte target, against ASCII
    // and non-ASCII literals: the mixed representations take the matcher's
    // own character test.
    let text: &[u8] = b"ab\xe9AB\xc9ab\xff\x80abc";
    for expression in ["ab", "AB", "é", "É", "b\u{e9}a", "c"] {
        for case_fold in [false, true] {
            let pattern = compile(expression, case_fold, false);
            for start in 0..=text.len() {
                let range = (text.len() - start) as isize;
                assert_eq!(
                    optimized(&pattern, text, start, range).0,
                    matcher_only(&pattern, text, start, range),
                    "expression={expression:?} case_fold={case_fold} start={start}"
                );
            }
        }
    }
}

#[test]
fn whole_literal_leaves_backward_and_long_folded_searches_to_the_matcher() {
    crate::test_utils::init_test_tracing();
    let long = format!("{}Buffer-Sub", "x".repeat(1000));
    let folded = compile("buffer-sub", true, true);
    assert_eq!(
        optimized(&folded, long.as_bytes(), 0, long.len() as isize).0,
        matcher_only(&folded, long.as_bytes(), 0, long.len() as isize)
    );
    // A case-sensitive literal keeps the substring scan at any length.
    let exact = compile("Buffer-Sub", false, true);
    let (found, entries) = optimized(&exact, long.as_bytes(), 0, long.len() as isize);
    assert_eq!(found, Some((1000, vec![1000], vec![1010])));
    assert_eq!(entries, 0);
    // Backward: from the end toward the start.
    let text = "abc abc";
    let start = text.len();
    assert_eq!(
        optimized(&exact, text.as_bytes(), start, -(start as isize)).0,
        matcher_only(&exact, text.as_bytes(), start, -(start as isize))
    );
    let pattern = compile("abc", false, true);
    assert_eq!(
        optimized(&pattern, text.as_bytes(), start, -(start as isize)).0,
        Some((4, vec![4], vec![7]))
    );
}
