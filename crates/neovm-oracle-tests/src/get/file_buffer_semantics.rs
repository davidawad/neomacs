//! Oracle parity tests for `get-file-buffer`.
//!
//! GNU implements `get-file-buffer` in `src/buffer.c` via `Fget_file_buffer`,
//! which expands the filename and scans live buffers for a matching `buffer-file-name`.

use crate::common::return_if_neovm_enable_oracle_proptest_not_set;

use crate::common::{assert_err_kind, assert_ok_eq, eval_oracle_and_neovm};

#[test]
fn oracle_get_file_buffer_nil_for_nonexistent_file() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let expect = expect_test::expect![[r#""OK nil""#]];
    let (oracle, neovm) = crate::common::eval_oracle_and_neovm_expect(
        "(get-file-buffer \"/nonexistent/path/to/nowhere.txt\")",
        expect,
    );
    assert_ok_eq("nil", &oracle, &neovm);
}

#[test]
fn oracle_get_file_buffer_requires_string_arg() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let expect = expect_test::expect![[r#""ERR (wrong-type-argument stringp 42)""#]];
    let (oracle, neovm) =
        crate::common::eval_oracle_and_neovm_expect("(get-file-buffer 42)", expect);
    assert_err_kind(&oracle, &neovm, "wrong-type-argument");
}

#[test]
fn oracle_get_file_buffer_wrong_number_of_args() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let expect = expect_test::expect![[r#""ERR (wrong-number-of-arguments get-file-buffer 0)""#]];
    let (oracle, neovm) = crate::common::eval_oracle_and_neovm_expect("(get-file-buffer)", expect);
    assert_err_kind(&oracle, &neovm, "wrong-number-of-arguments");

    let expect = expect_test::expect![[r#""ERR (wrong-number-of-arguments get-file-buffer 2)""#]];
    let (oracle2, neovm2) =
        crate::common::eval_oracle_and_neovm_expect("(get-file-buffer \"a\" \"b\")", expect);
    assert_err_kind(&oracle2, &neovm2, "wrong-number-of-arguments");
}

#[test]
fn oracle_get_file_buffer_compares_expanded_names_without_resolving_symlinks() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    // Two buffers, one file: REAL and a symlink LINK to it. GNU's
    // `Fget_file_buffer` (src/buffer.c:493) compares the expansion of its
    // argument with each `buffer-file-name` as stored, and never resolves a
    // symlink or expands the stored name.
    let form = r#"
(let* ((root (file-name-as-directory
              (make-temp-file "neomacs-oracle-get-file-buffer-" t)))
       (real (expand-file-name "real.txt" root))
       (link (expand-file-name "link.txt" root))
       (a (generate-new-buffer " gfb-real"))
       (b (generate-new-buffer " gfb-link"))
       (c (generate-new-buffer " gfb-relative")))
  (unwind-protect
      (progn
        (write-region "x" nil real nil 'silent)
        (make-symbolic-link real link)
        (with-current-buffer a (setq buffer-file-name real))
        (with-current-buffer c (setq buffer-file-name "relative.txt"))
        (list
         (eq (get-file-buffer real) a)
         ;; Only REAL is visited: the link's name matches no buffer.
         (get-file-buffer link)
         (progn
           (with-current-buffer b (setq buffer-file-name link))
           (list (eq (get-file-buffer real) a)
                 (eq (get-file-buffer link) b)))
         (let ((default-directory root))
           (list (eq (get-file-buffer "real.txt") a)
                 (eq (get-file-buffer "./sub/../link.txt") b)
                 ;; C's relative name is compared as stored.
                 (get-file-buffer "relative.txt")
                 ;; An absolute argument is expanded too.
                 (eq (get-file-buffer (concat root "nosuch/../real.txt")) a)))))
    (kill-buffer a)
    (kill-buffer b)
    (kill-buffer c)
    (delete-directory root t)))
"#;

    let expect = expect_test::expect![[r#""OK (t nil (t t) (t t nil t))""#]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

#[test]
fn oracle_find_buffer_visiting_finds_the_symlinked_buffer_by_truename() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    // `get-file-buffer' misses the symlink's name, so `find-buffer-visiting'
    // (lisp/files.el) falls through to its truename and file-number checks,
    // and `find-file-noselect' reuses the buffer visiting REAL.
    let form = r#"
(let* ((root (file-name-as-directory
              (make-temp-file "neomacs-oracle-find-buffer-visiting-" t)))
       (real (expand-file-name "real.txt" root))
       (link (expand-file-name "link.txt" root))
       (find-file-suppress-same-file-warnings t)
       (buf nil))
  (unwind-protect
      (progn
        (write-region "x" nil real nil 'silent)
        (make-symbolic-link real link)
        (setq buf (find-file-noselect real))
        (list
         (get-file-buffer link)
         (eq (find-buffer-visiting link) buf)
         (eq (find-file-noselect link) buf)
         (equal (buffer-local-value 'buffer-file-name buf) real)))
    (when (buffer-live-p buf) (kill-buffer buf))
    (delete-directory root t)))
"#;

    let expect = expect_test::expect![[r#""OK (nil t t t)""#]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

#[test]
fn oracle_get_file_buffer_consults_file_name_handlers() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    // GNU expands FILENAME (calling the `expand-file-name' handler), then asks
    // the `get-file-buffer' handler, and returns its answer only when it is a
    // buffer.
    let form = r#"
(let* ((ops nil)
       (buf (generate-new-buffer " gfb-handler"))
       (file-name-handler-alist
        (list (cons "\\`/gfb-fake:"
                    (lambda (op &rest args)
                      (setq ops (cons op ops))
                      (cond
                       ((eq op 'expand-file-name) (car args))
                       ((eq op 'get-file-buffer)
                        (if (string-suffix-p "hit" (car args)) buf 'not-a-buffer))))))))
  (unwind-protect
      (list (eq (get-file-buffer "/gfb-fake:hit") buf)
            (get-file-buffer "/gfb-fake:miss")
            (reverse ops))
    (kill-buffer buf)))
"#;

    let expect = expect_test::expect![[
        r#""OK (t nil (expand-file-name get-file-buffer expand-file-name get-file-buffer))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}
