//! Oracle parity tests for an Emacs started from a user's
//! `dump-emacs-portable` image.
//!
//! GNU's pdumper loads an image as-is (src/pdumper.c `pdumper_load`): the
//! restored session's `features` are exactly the dumped ones, so a library
//! loaded before the dump is not loaded again by a later `require`.  The form
//! dumps a child Emacs of the binary under test and starts a second child
//! from that image.

use crate::common::return_if_neovm_enable_oracle_proptest_not_set;

#[test]
fn oracle_user_dump_keeps_the_features_it_was_dumped_with() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(let* ((root (make-temp-file "neomacs-oracle-user-dump-" t))
       (dump (expand-file-name "user.pdmp" root))
       (emacs (expand-file-name invocation-name invocation-directory))
       (libraries '(cl-lib cl-macs cl-seq cl-extra gv pcase rx icons)))
  (unwind-protect
      (list
       (call-process emacs nil nil nil "-Q" "--batch" "--eval"
                     (format "(progn (mapc #'require '%S) (dump-emacs-portable %S))"
                             libraries dump))
       (with-temp-buffer
         (call-process emacs nil t nil "-Q" "--batch" "--dump-file" dump "--eval"
                       (format "(prin1 (mapcar #'featurep '%S))" libraries))
         (car (read-from-string (buffer-string)))))
    (delete-directory root t)))
"#;

    let expect = expect_test::expect![[r#""OK (0 (t t t t t t t t))""#]];
    crate::common::assert_oracle_parity_expect(form, expect);
}
