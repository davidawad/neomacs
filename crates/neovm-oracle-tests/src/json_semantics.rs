//! Oracle parity tests for GNU JSON primitive option semantics.
//!
//! GNU implements `json-serialize` and `json-parse-string` in `src/json.c`.
//! Its `json_parse_args` walks keyword pairs from right to left so duplicate
//! keyword values that appear first take precedence.

use crate::common::assert_oracle_parity;
use crate::common::return_if_neovm_enable_oracle_proptest_not_set;

#[test]
fn oracle_json_duplicate_keyword_options_use_first_value_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(list
 (condition-case err
     (json-serialize :a :null-object :a :null-object :b)
   (error (list (car err) (cdr err))))
 (condition-case err
     (json-serialize :b :null-object :a :null-object :b)
   (error (list (car err) (cdr err))))
 (condition-case err
     (json-parse-string
      "[null,false]"
      :array-type 'list
      :null-object 'first-null
      :null-object 'second-null
      :false-object 'first-false
      :false-object 'second-false)
   (error (list (car err) (cdr err)))))
"#;

    let expect = expect_test::expect![[
        r#""OK (\"null\" (wrong-type-argument (json-value-p :b)) (first-null first-false))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

#[test]
fn oracle_json_serialize_default_null_and_false_sentinels_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(list
 (condition-case err
     (json-serialize nil)
   (error (list (car err) (cdr err))))
 (condition-case err
     (json-serialize :null)
   (error (list (car err) (cdr err))))
 (condition-case err
     (json-serialize :false)
   (error (list (car err) (cdr err))))
 (condition-case err
     (json-serialize :json-false)
   (error (list (car err) (cdr err)))))
"#;

    let expect = expect_test::expect![[
        r#""OK (\"{}\" \"null\" \"false\" (wrong-type-argument (json-value-p :json-false)))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU 31.1 has no `json-serialize-error`: serializer failures use plain
/// `error` or `wrong-type-argument` conditions. The depth limit is
/// `json_out_nest`'s `maxdepth = 50` (the 51st container errors), and the
/// symbol is not interned at boot.
#[test]
fn oracle_json_serialize_error_conditions_match_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(let ((nested (lambda (n) (let ((v 1)) (dotimes (_ n) (setq v (vector v))) v))))
  (list
   (condition-case e (length (json-serialize (funcall nested 50)))
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize (funcall nested 51))
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize 0.0e+NaN)
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize 1.0e+INF)
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize '(1 2))
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize '(a . 1))
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize '((a . 1) . 2))
     (error (list (car e) (cdr e))))
   (condition-case e (json-serialize (make-marker))
     (error (list (car e) (cdr e))))
   (intern-soft "json-serialize-error")
   (and (intern-soft "json-error") t)
   (condition-case e (with-temp-buffer (json-insert (funcall nested 51)))
     (error (list (car e) (cdr e))))
   (condition-case e (with-temp-buffer (json-insert 0.0e+NaN))
     (error (list (car e) (cdr e))))))
"#;

    let expect = expect_test::expect![[
        r#""OK (101 (error (\"Maximum JSON serialization depth exceeded\")) (error (\"JSON does not allow Inf or NaN\" 0.0e+NaN)) (error (\"JSON does not allow Inf or NaN\" 1.0e+INF)) (wrong-type-argument (symbolp 1)) (wrong-type-argument (consp 1)) (wrong-type-argument (listp ((a . 1) . 2))) (wrong-type-argument (json-value-p #<marker in no buffer>)) nil t (error (\"Maximum JSON serialization depth exceeded\")) (error (\"JSON does not allow Inf or NaN\" 0.0e+NaN)))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU's parser (src/json.c `json_parse_array`, `json_parse_object`) builds
/// containers from one object workspace and interns each alist/plist key
/// (`json_parse_string` with `intern`), so a repeated key is one symbol;
/// a repeated hash-table member keeps its first position and its last value.
/// Every string it builds is multibyte (`make_multibyte_string`).
#[test]
fn oracle_json_parse_containers_and_repeated_keys_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(let* ((json "[{\"label\":\"a\",\"kind\":1},{\"kind\":2.5,\"label\":\"b\\u00e9\\n\"},{\"\":[],\"é\":{},\"n\":[null,false,true]},{\"x\":1,\"y\":[{\"x\":[2,{}]}],\"x\":3}]")
       (pl (json-parse-string json :object-type 'plist :array-type 'list))
       (al (json-parse-string json :object-type 'alist))
       (ht (json-parse-string json)))
  (list pl al
        (eq (car (nth 0 pl)) (nth 2 (nth 1 pl)))
        (eq (car (car (aref al 0))) (car (cadr (aref al 1))))
        (mapcar (lambda (table)
                  (let (members)
                    (maphash (lambda (k v) (push (list k v (multibyte-string-p k)) members)) table)
                    (nreverse members)))
                (list (aref ht 1) (aref ht 3)))
        (mapcar #'multibyte-string-p
                (list (plist-get (nth 0 pl) :label) (plist-get (nth 1 pl) :label)
                      (json-parse-string "\"\"")))
        (json-serialize al)))
"#;

    let expect = expect_test::expect![[
        r#""OK (((:label \"a\" :kind 1) (:kind 2.5 :label \"bé\\n\") (: nil :é nil :n (:null :false t)) (:x 1 :y ((:x (2 nil))) :x 3)) [((label . \"a\") (kind . 1)) ((kind . 2.5) (label . \"bé\\n\")) ((## . []) (é) (n . [:null :false t])) ((x . 1) (y . [((x . [2 nil]))]) (x . 3))] t t (((\"kind\" 2.5 t) (\"label\" \"bé\\n\" t)) ((\"x\" 3 t) (\"y\" [#s(hash-table test equal data (\"x\" [2 #s(hash-table test equal)]))] t))) (t t t) \"[{\\\"label\\\":\\\"a\\\",\\\"kind\\\":1},{\\\"kind\\\":2.5,\\\"label\\\":\\\"bé\\\\n\\\"},{\\\"\\\":[],\\\"é\\\":{},\\\"n\\\":[null,false,true]},{\\\"x\\\":1,\\\"y\\\":[{\\\"x\\\":[2,{}]}]}]\")""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU `json_parse_string` copies plain ASCII in runs and stops for quotes,
/// escapes, control characters and UTF-8 leads; strings of every length
/// around a word boundary come back with GNU's text, length, byte count and
/// multibyte flag, and malformed ones with GNU's condition and position.
#[test]
fn oracle_json_parse_string_runs_and_escapes_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(mapcar
 (lambda (json)
   (condition-case err
       (let ((s (json-parse-string json)))
         (list s (length s) (string-bytes s) (multibyte-string-p s)))
     (error err)))
 (list "\"\"" "\"a\"" "\"abcdefg\"" "\"abcdefgh\"" "\"abcdefghi\""
       "\"0123456789abcdef\\n0123456789abcdef\""
       "\"caf\\u00e9 au lait\"" "\"café au lait\"" "\"日本語 and more ASCII\""
       "\"\\uD83D\\uDE00!\"" "\"q\\\"uote\\\\slash\\/\""
       "\"abcdefgh" "\"abcdefgh\nij\"" "\"abcdefgh\\x\""
       "\"abcdefgh\377\""
       "\"del\177del\""))
"#;

    let expect = expect_test::expect![[
        r#""OK ((\"\" 0 0 t) (\"a\" 1 1 t) (\"abcdefg\" 7 7 t) (\"abcdefgh\" 8 8 t) (\"abcdefghi\" 9 9 t) (\"0123456789abcdef\\n0123456789abcdef\" 33 33 t) (\"café au lait\" 12 13 t) (\"café au lait\" 12 13 t) (\"日本語 and more ASCII\" 18 24 t) (\"😀!\" 2 5 t) (\"q\\\"uote\\\\slash/\" 13 13 t) (json-end-of-file 1 nil 9) (json-parse-error 2 nil 10) (json-escape-sequence-error 1 nil 11) (json-utf8-decode-error 1 nil 11) (\"del\u{7f}del\" 7 7 t))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU reads a string's UTF-8 strictly and byte by byte (`json_parse_string`,
/// the `c & 0x80` branch): a character cut off by the end of input is
/// `json-end-of-file`, a malformed one `json-utf8-decode-error` with the
/// usual (LINE nil POS) data, POS just after the last byte read, in unibyte
/// and multibyte strings and in buffers.
#[test]
fn oracle_json_parse_utf8_errors_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(list
 (mapcar
  (lambda (json)
    (condition-case err (json-parse-string json) (error err)))
  (list "\"ok\303\251\"" "\"\303\"" "\"\303" "\"\300\200\"" "\"\355\240\200\""
        "\"\364\220\200\200\"" "\"\370\200\200\"" "\"\200\"" "\"\342\202\""
        "\"\360\237\230\200\"" "\"\365\200\200\200\""
        (string-to-multibyte "\"x\377\"") "\"é\303\""))
 (with-temp-buffer
   (set-buffer-multibyte nil)
   (insert "\"ab\377\" rest")
   (goto-char 1)
   (list (condition-case err (json-parse-buffer) (error err)) (point))))
"#;

    let expect = expect_test::expect![[
        r#""OK ((\"oké\" (json-utf8-decode-error 1 nil 3) (json-end-of-file 1 nil 2) (json-utf8-decode-error 1 nil 3) (json-utf8-decode-error 1 nil 4) (json-utf8-decode-error 1 nil 5) (json-utf8-decode-error 1 nil 3) (json-utf8-decode-error 1 nil 3) (json-utf8-decode-error 1 nil 4) \"😀\" (json-utf8-decode-error 1 nil 5) (json-utf8-decode-error 1 nil 3) (json-utf8-decode-error 1 nil 3)) ((json-utf8-decode-error 1 nil 5) 1))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU `Fjson_parse_buffer` reads the text on both sides of the gap. With
/// the gap inside the unread text, the value, the point it leaves and the
/// end-of-input position under narrowing are those of any other parse.
#[test]
fn oracle_json_parse_buffer_across_the_gap_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(with-temp-buffer
  (insert "[\"é\",{\"k\":[1,2]},\"ok\"] tail")
  (goto-char 6)
  (insert "0,")
  (goto-char (point-min))
  (list (json-parse-buffer :object-type 'plist :array-type 'list)
        (point)
        (buffer-substring-no-properties (point) (point-max))
        (progn
          (goto-char 6)
          (insert " ")
          (goto-char (point-min))
          (save-restriction
            (narrow-to-region 1 12)
            (list (condition-case err (json-parse-buffer) (error err))
                  (point))))
        (progn
          (erase-buffer)
          (insert "[1,2]  {\"next\":[true]} ")
          (goto-char 4)
          (insert "0,")
          (goto-char (point-min))
          (list (json-parse-buffer) (point)
                (json-parse-buffer :object-type 'alist) (point)
                (condition-case err (json-parse-buffer) (error err)) (point)))))
"#;

    let expect = expect_test::expect![[
        r#""OK ((\"é\" 0 (:k (1 2)) \"ok\") 25 \" tail\" ((json-end-of-file 1 nil 11) 1) ([1 0 2] 8 ((next . [t])) 25 (json-end-of-file 1 nil 1) 25))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU interns every alist and plist key in the obarray
/// (`intern_c_multibyte`, src/lread.c), before an error too: `intern-soft`
/// finds a key the document introduced, and after `unintern` the next parse
/// returns the fresh symbol `intern` makes.
#[test]
fn oracle_json_parse_object_keys_are_obarray_members_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(list
 (list (progn (json-parse-string "{\"jm-fresh-alist-key\":1}" :object-type 'alist)
              (intern-soft "jm-fresh-alist-key"))
       (progn (json-parse-string "{\"jm-fresh-plist-key\":1}" :object-type 'plist)
              (intern-soft ":jm-fresh-plist-key"))
       (progn (condition-case nil
                  (json-parse-string "{\"jm-before-error\":1,}" :object-type 'alist)
                (json-parse-error nil))
              (intern-soft "jm-before-error"))
       (with-temp-buffer
         (insert "{\"jm-buffer-key\":1}")
         (goto-char (point-min))
         (json-parse-buffer :object-type 'alist)
         (intern-soft "jm-buffer-key")))
 (let* ((parse (lambda (type)
                 (let ((v (json-parse-string "{\"jm-key\":1}" :object-type type)))
                   (if (eq type 'alist) (caar v) (car v)))))
        (a (funcall parse 'alist))
        (ka (funcall parse 'plist)))
   (unintern "jm-key" obarray)
   (unintern ":jm-key" obarray)
   (let ((b (funcall parse 'alist))
         (kb (funcall parse 'plist)))
     (list (eq a b) (eq b (intern-soft "jm-key"))
           (eq ka kb) (eq kb (intern-soft ":jm-key"))))))
"#;

    let expect = expect_test::expect![[
        r#""OK ((jm-fresh-alist-key :jm-fresh-plist-key jm-before-error jm-buffer-key) (nil t nil t))""#
    ]];
    crate::common::assert_oracle_parity_expect(form, expect);
}

/// GNU `json_parse_number` gives the integer a literal denotes: a fixnum
/// while it fits, a bignum past `most-positive-fixnum`, at every length.
#[test]
fn oracle_json_parse_integer_literals_like_gnu() {
    return_if_neovm_enable_oracle_proptest_not_set!();

    let form = r#"
(mapcar
 (lambda (literal)
   (let ((n (aref (json-parse-string (concat "[" literal "]")) 0)))
     (list n (fixnump n))))
 '("0" "-0" "7" "-1" "123456789012345678" "-999999999999999999"
   "1234567890123456789" "-1234567890123456789" "2305843009213693951"
   "-2305843009213693952" "2305843009213693952" "-2305843009213693953"
   "123456789012345678901234567890" "1.5" "-0.0" "1e3"))
"#;

    // Raw: the normalized program squashes fixnums above 10^12 to 0.
    let expect = expect_test::expect![[
        r#""OK ((0 t) (0 t) (7 t) (-1 t) (123456789012345678 t) (-999999999999999999 t) (1234567890123456789 t) (-1234567890123456789 t) (2305843009213693951 t) (-2305843009213693952 t) (2305843009213693952 nil) (-2305843009213693953 nil) (123456789012345678901234567890 nil) (1.5 nil) (-0.0 nil) (1000.0 nil))""#
    ]];
    crate::common::assert_oracle_parity_with_load_raw_expect(form, &[], expect);
}
