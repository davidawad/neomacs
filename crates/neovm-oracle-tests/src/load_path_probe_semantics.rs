//! Oracle parity tests for `load`/`require` lookups across changes to a
//! `load-path` directory within one session.
//!
//! GNU `openp` (src/lread.c) opens every suffixed candidate on every lookup,
//! so a file created after a failed lookup is found by the next one and a
//! deleted file stops being found.  Neomacs answers misses from a directory
//! listing trusted only while the directory is unchanged and settled for two
//! seconds; the sleeps below age the directory past that window so the
//! lookups really go through a listing, and the creations and deletions show
//! the listing is reread when the directory changes.

use crate::common::return_if_neovm_enable_oracle_proptest_not_set;

#[test]
fn oracle_load_path_lookup_sees_files_created_and_deleted_between_lookups() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(let* ((root (make-temp-file "neomacs-oracle-probe-" t))
       (d1 (expand-file-name "d1" root))
       (d2 (expand-file-name "d2" root))
       ;; The system directories stay behind them: a miss searches all of it.
       (load-path (append (list d1 d2) load-path))
       (rel (lambda (file) (and file (file-relative-name file root))))
       (write (lambda (dir name text)
                (write-region text nil (expand-file-name name dir) nil 'silent)))
       (find (lambda (name)
               (funcall rel (locate-file name (list d1 d2) (get-load-suffixes)))))
       results)
  (unwind-protect
      (progn
        (make-directory d1)
        (make-directory d2)
        (funcall write d1 "probe-a.el" "(setq probe-a 'el)")
        (funcall write d2 "probe-b.el" "(provide 'probe-b)")
        ;; Let both directories settle so their listings are trusted.
        (sleep-for 2.5)
        (push (list (load "probe-a" t t) probe-a) results)
        (push (condition-case err (load "probe-c" nil t) (error (car err))) results)
        (push (require 'probe-b nil t) results)
        (push (require 'probe-d nil t) results)
        ;; Create files after failed lookups, delete one that was found,
        ;; and add a compiled twin that now wins the suffix order.
        (funcall write d1 "probe-c.el" "(setq probe-c 'created)")
        (funcall write d2 "probe-d.el" "(provide 'probe-d)")
        ;; A real compiled twin: compile a variant, then put the source back.
        (funcall write d1 "probe-a.el" ";;; -*- lexical-binding: t -*-\n(setq probe-a 'elc)")
        (let ((inhibit-message t))
          (byte-compile-file (expand-file-name "probe-a.el" d1)))
        (funcall write d1 "probe-a.el" "(setq probe-a 'el)")
        (push (list (load "probe-c" nil t) probe-c) results)
        (push (require 'probe-d nil t) results)
        (push (list (load "probe-a" nil t) probe-a) results)
        (delete-file (expand-file-name "probe-c.el" d1))
        (push (condition-case err (load "probe-c" nil t) (error (car err))) results)
        ;; Again after the directories have settled with their new contents.
        (sleep-for 2.5)
        (funcall write d2 "probe-e.el" "(setq probe-e t)")
        (push (list (funcall find "probe-a") (funcall find "probe-c")
                    (funcall find "probe-d") (funcall find "probe-e"))
              results)
        (delete-file (expand-file-name "probe-a.elc" d1))
        (push (list (load "probe-a" nil t) probe-a (funcall find "probe-a"))
              results)
        (nreverse results))
    (delete-directory root t)))
"#;

    let expect = expect_test::expect![[
        r#""OK ((t el) file-missing probe-b nil (t created) probe-d (t elc) file-missing (\"d1/probe-a.elc\" nil \"d2/probe-d.el\" \"d2/probe-e.el\") (t el \"d1/probe-a.el\"))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}
