//! GNU parity for literal regexps in `string-match` and in
//! `completion-regexp-list`.
//!
//! A regexp that is one literal run (`regexp-quote` output, what
//! `orderless-literal` emits) is searched without the matcher, and an
//! `all-completions` call whose input extends the previous call's tests only
//! that call's survivors. GNU `match_regexps` (`src/minibuf.c`) tests every
//! candidate with `fast_string_match_internal`, which sets no match data; these
//! probes pin what must not change: which strings match, where, under which
//! case folding and representation; that a candidate string mutated in place
//! or a collection edited between two calls is seen fresh; that an invalid
//! regexp signals only when a candidate reaches it; that the predicate sees
//! exactly the regexp survivors, in order; and that match data is untouched.

use crate::common::{assert_oracle_parity_expect, return_if_neovm_enable_oracle_proptest_not_set};

/// Literal regexps (and the near-literals beside them) over multibyte, unibyte
/// and case-partner text, both foldings, two start positions, with match data.
#[test]
fn oracle_prop_string_match_literal_regexps() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"(let ((texts (list "find-file-other-window-buffer-substring-no-properties"
                   "Buffer-Sub BUFFER-SUB buffer-sub"
                   "aKelvin Kelvin kELVIN" "Straße STRASSE strasse"
                   "ΣσςΣ xσq XΣQ" "éЖq😀Жq жQ" (string-to-unibyte "ab\351AB\311ab")
                   "a.b a*b ^def def$ [x \\ q?" "line1\nbuffer-sub\n"))
      (res nil) (hits 0))
  (dolist (case-fold-search '(nil t))
    (dolist (re '("buffer-sub" "BUFFER" "kelvin" "straße" "σq" "Σq" "жq" "Жq" "é"
                  "a\\.b" "a\\*b" "\\^def" "def\\$" "\\[x" "\\\\ q\\?" "b" "-sub\n"))
      (dolist (s texts)
        (dolist (start '(0 3))
          (let ((m (condition-case err (string-match re s start) (error (car err)))))
            (when (integerp m) (setq hits (1+ hits)))
            (push (list m (match-data)) res))))))
  (list hits (md5 (prin1-to-string (nreverse res)))))"#;

    let expect = expect_test::expect![[r#""OK (83 \"b083c222103fe1d3c1bc4739879e7413\")""#]];
    assert_oracle_parity_expect(form, expect);
}

/// Keystroke-by-keystroke queries as `orderless-literal` builds them: each call
/// extends the previous input, under both `completion-ignore-case` settings.
#[test]
fn oracle_prop_all_completions_literal_regexps_extend_input() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"(let* ((cands (list "buffer-substring" "Buffer-Substring-No-Properties" "buffer-string"
                    "insert-buffer-substring" "bufferp" "BUFFER" "find-file" "font-lock-face"
                    "font-lock-keyword-face" "Kelvin" "\u212Aelvin" "Straße" "ΣΣ-σq" "жq-Жq"
                    (string-to-unibyte "ab\351c") 'buffer-sym "x"))
       (coll (append cands (list '("buffer-alist-key" . 1) 42)))
       (counts nil) (out nil))
  (dolist (completion-ignore-case '(nil t))
    (dolist (query '("buffer-sub str" "font lock face" "kelvin" "σq" "жq" "BUF sub"))
      (dotimes (i (length query))
        (let* ((input (substring query 0 (1+ i)))
               (completion-regexp-list (mapcar #'regexp-quote (split-string input " " t)))
               (all (all-completions "" coll)))
          (push (length all) counts)
          (push (cons input all) out)))))
  (list (nreverse counts) (md5 (prin1-to-string (nreverse out)))))"#;

    let expect = expect_test::expect![[
        r#""OK ((8 6 6 6 6 6 5 4 2 2 2 2 2 2 10 2 2 2 2 2 2 2 2 2 2 2 2 2 3 2 0 0 0 0 1 1 1 1 2 1 1 1 0 0 0 9 8 8 8 8 8 6 5 3 3 3 3 3 3 11 2 2 2 2 2 2 2 2 2 2 2 2 2 4 3 1 1 1 1 1 1 1 1 9 8 8 8 6 3 3) \"9a5983a440b76d23b9029d52832a0a78\")""#
    ]];
    assert_oracle_parity_expect(form, expect);
}

/// A candidate mutated in place, a collection edited, reordered or re-cased
/// between two narrowing calls is matched as it is now.
#[test]
fn oracle_prop_all_completions_sees_mutation_between_calls() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"(let* ((s (copy-sequence "bxfferzz")) (coll (list "buffer" s "buff")) r)
  (let ((completion-regexp-list '("bu"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (aset s 1 ?u)
  (let ((completion-regexp-list '("buf"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (push (eq (nth 1 (let ((completion-regexp-list '("buff"))) (all-completions "" coll))) s) r)
  (setcdr (cddr coll) (list "bufz"))
  (let ((completion-regexp-list '("buf"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-regexp-list '("buff"))) (push (mapcar (function copy-sequence) (all-completions "" (reverse coll))) r))
  (push (mapcar (function copy-sequence) (all-completions "b" coll)) r)
  (push (mapcar (function copy-sequence) (all-completions "bu" coll)) r)
  (let ((completion-regexp-list '("BU"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-ignore-case t) (completion-regexp-list '("BUF"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-ignore-case t) (completion-regexp-list '("BUFF"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-ignore-case t) (completion-regexp-list '("BUFF"))) (push (mapcar (function copy-sequence) (all-completions "B" coll)) r))
  (let ((completion-ignore-case t) (completion-regexp-list '("BUFFE"))) (push (mapcar (function copy-sequence) (all-completions "BU" coll)) r))
  (aset s 0 ?B)
  (let ((completion-ignore-case nil) (completion-regexp-list '("buffe"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-ignore-case nil) (completion-regexp-list '("buffer"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (let ((completion-ignore-case nil) (completion-regexp-list '("uffer"))) (push (mapcar (function copy-sequence) (all-completions "" coll)) r))
  (nreverse r))"#;

    let expect = expect_test::expect![[
        r#""OK ((\"buffer\" \"buff\") (\"buffer\" \"bufferzz\" \"buff\") t (\"buffer\" \"bufferzz\" \"buff\" \"bufz\") (\"buff\" \"bufferzz\" \"buffer\") (\"buffer\" \"bufferzz\" \"buff\" \"bufz\") (\"buffer\" \"bufferzz\" \"buff\" \"bufz\") nil (\"buffer\" \"bufferzz\" \"buff\" \"bufz\") (\"buffer\" \"bufferzz\" \"buff\") (\"buffer\" \"bufferzz\" \"buff\") (\"buffer\" \"bufferzz\") (\"buffer\") (\"buffer\") (\"buffer\" \"Bufferzz\"))""#
    ]];
    assert_oracle_parity_expect(form, expect);
}

/// Lazy regexp errors, untouched match data, try/test-completion, alternations
/// that are not literals, the predicate's view, and obarray collections.
#[test]
fn oracle_prop_completion_regexps_errors_predicate_and_match_data() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"(list
 (let ((completion-regexp-list '("zzz" "\\("))) (all-completions "" '("abc" "def")))
 (condition-case e (let ((completion-regexp-list '("a" "\\("))) (all-completions "" '("abc"))) (error e))
 (let ((completion-regexp-list '("\\("))) (all-completions "x" '("abc")))
 (progn (string-match "\\(b\\)" "abc")
        (let ((completion-regexp-list '("c"))) (all-completions "" '("abc" "c")))
        (match-data))
 (let ((completion-regexp-list '("b")))
   (list (try-completion "a" '("abc" "abd" "ax")) (test-completion "abc" '("abc"))
         (test-completion "ax" '("ax"))))
 (let ((completion-regexp-list '("\\^d\\|^d"))) (all-completions "" '("def" "^d" "xdef" "a\n^dx" "dd")))
 (let ((completion-regexp-list '("\\^d\\|^d"))) (all-completions "" '("def" "^d" "xdef" "a\n^dx" "dd")))
 (let ((completion-regexp-list '("\\^de\\|^de"))) (all-completions "" '("def" "^d" "xdef" "a\n^dex" "dd")))
 (let (seen)
   (dolist (in '("b" "bu" "buf"))
     (let ((completion-regexp-list (list in)))
       (push (all-completions "" '("abc" "buf" "bxf" "ebuf") (lambda (c) (push c seen) t)) seen)))
   (nreverse seen))
 (let ((ob (obarray-make)))
   (dolist (n '("buffer-a" "buffer-b" "bufx" "zz" "Buffer-c")) (intern n ob))
   (list (sort (let ((completion-regexp-list '("buf"))) (all-completions "" ob)) #'string<)
         (sort (let ((completion-regexp-list '("buffer"))) (all-completions "" ob)) #'string<)
         (sort (let ((completion-regexp-list '("buffer-"))) (all-completions "b" ob)) #'string<))))"#;

    let expect = expect_test::expect![[
        r#""OK (nil (invalid-regexp \"Unmatched ( or \\\\(\") nil (1 2 1 2) (\"ab\" t nil) (\"def\" \"^d\" \"a\\n^dx\" \"dd\") (\"def\" \"^d\" \"a\\n^dx\" \"dd\") (\"def\" \"a\\n^dex\") (\"abc\" \"buf\" \"bxf\" \"ebuf\" (\"abc\" \"buf\" \"bxf\" \"ebuf\") \"buf\" \"ebuf\" (\"buf\" \"ebuf\") \"buf\" \"ebuf\" (\"buf\" \"ebuf\")) ((\"buffer-a\" \"buffer-b\" \"bufx\") (\"buffer-a\" \"buffer-b\") (\"buffer-a\" \"buffer-b\")))""#
    ]];
    assert_oracle_parity_expect(form, expect);
}
