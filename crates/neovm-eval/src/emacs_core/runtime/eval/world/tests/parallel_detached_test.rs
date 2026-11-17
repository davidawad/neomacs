//! Detached execution in a Parallel World (slice S3): Lisp threads run
//! Scalar/Sealed-class code without the Baton and attach only for
//! `World`-class primitives, heap accesses and collections.

use super::parallel_classify::{Placement, needs_attach, placement};
use super::*;
use crate::emacs_core::eval::ThreadingMode;
use crate::emacs_core::print::print_value;
use neovm_value::detached::AttachReason;
use neovm_value::value::Value;

fn eval(ev: &mut Context, src: &str) -> String {
    match ev.eval_str(src) {
        Ok(value) => print_value(&value),
        Err(neovm_value::error::EvalError::Signal { symbol, data, .. }) => format!(
            "ERR {} {}",
            neovm_value::intern::resolve_sym(symbol),
            print_value(&Value::list(data))
        ),
        Err(error) => format!("ERR {error:?}"),
    }
}

/// A Parallel World evaluating with lexical binding: a dynamic `let` of
/// an unprojected symbol binds the shared canonical cell, which threads
/// running at once cannot share (see VERIFY.md, stage-2 limitations).
fn parallel() -> Context {
    crate::test_utils::init_test_tracing();
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::force_jit_off_for_test(true);
    let mut ev =
        Context::new_with_threading(ThreadingMode::Parallel, crate::test_utils::unit_runtime());
    ev.set_lexical_binding(true);
    ev
}

fn stats(ev: &mut Context) -> ParallelStats {
    ev.parallel_lock()
        .expect("a started Parallel World")
        .stats()
}

/// The numeric loop of the measurement, run serially in Rust.
fn numeric_loop(iterations: i64) -> i64 {
    (0..iterations).fold(0, |sum, i| (sum + i * i) & 0xff_ffff)
}

#[test]
fn the_effect_classes_place_primitives() {
    assert_eq!(placement("car"), Placement::Detached, "Sealed");
    assert_eq!(placement("consp"), Placement::Detached, "Scalar");
    assert_eq!(
        placement("cons"),
        Placement::Detached,
        "the cell attaches itself"
    );
    assert_eq!(
        placement("+"),
        Placement::DetachedIfNumeric,
        "Arith(Numeric)"
    );
    assert_eq!(placement("logand"), Placement::DetachedIfNumeric);
    assert_eq!(placement("funcall"), Placement::Detached);
    assert_eq!(placement("set"), Placement::Attach(AttachReason::Class));
    assert_eq!(placement("insert"), Placement::Attach(AttachReason::Class));
    assert_eq!(placement("setcar"), Placement::Attach(AttachReason::Alloc));
    assert_eq!(
        placement("make-string"),
        Placement::Attach(AttachReason::Alloc)
    );
    assert_eq!(placement("intern"), Placement::Attach(AttachReason::Intern));
    assert!(needs_attach("set") && !needs_attach("car") && !needs_attach("+"));
    assert_eq!(
        placement("no-such-primitive"),
        Placement::Attach(AttachReason::Class),
        "an unknown primitive is World"
    );
}

#[test]
fn every_override_names_an_inventory_primitive() {
    use crate::emacs_core::subr::effect_catalog::inventory;
    use crate::emacs_core::subr::effect_schema::CatalogKind;
    let names = [
        "setcar",
        "setcdr",
        "intern",
        "intern-soft",
        "read",
        "read-from-string",
        "unintern",
        "make-string",
        "string",
        "concat",
        "substring",
        "format",
        "number-to-string",
        "symbol-name",
        "make-vector",
        "vector",
        "vconcat",
        "make-bool-vector",
        "make-hash-table",
        "copy-hash-table",
        "puthash",
        "make-record",
        "record",
        "make-closure",
        "make-byte-code",
        "make-marker",
        "copy-sequence",
        "logand",
        "logior",
        "logxor",
        "lognot",
        "ash",
        "funcall",
        "apply",
        "eval",
    ];
    for name in names {
        assert!(
            inventory()
                .iter()
                .any(|entry| entry.kind == CatalogKind::Primitive && entry.name == name),
            "{name} is not an inventory primitive"
        );
    }
}

/// Item 7(a): two threads run the interpreted numeric loop detached.
#[test]
fn two_detached_numeric_loops_match_the_serial_result_without_attaching() {
    const ITERATIONS: i64 = 3_000_000;
    let mut ev = parallel();
    let form = format!(
        "(progn
           (fset 'tpd-loop
                 (lambda ()
                   (let ((s 0) (i 0))
                     (while (< i {ITERATIONS})
                       (setq s (logand (+ s (* i i)) #xffffff))
                       (setq i (1+ i)))
                     s)))
           (let ((a (make-thread 'tpd-loop))
                 (b (make-thread 'tpd-loop)))
             (list (thread-join a) (thread-join b))))"
    );
    let serial = numeric_loop(ITERATIONS);
    assert_eq!(eval(&mut ev, &form), format!("({serial} {serial})"));
    let after = stats(&mut ev);
    println!("parallel stats after 2 x {ITERATIONS} detached iterations: {after:?}");
    assert!(
        after.attach_count < 100,
        "the loop runs detached: {} attaches",
        after.attach_count
    );
}

/// Item 7(b): a canonical write attaches, once per `set`.
#[test]
fn a_detached_set_attaches_once_per_call() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (setq tpd-global-x nil)
               (let ((th (make-thread
                          (lambda ()
                            (let ((before (plist-get (neomacs-shared-heap-stats)
                                                     :attach-by-class))
                                  (k 0))
                              (while (< k 50)
                                (set 'tpd-global-x k)
                                (setq k (1+ k)))
                              (- (plist-get (neomacs-shared-heap-stats) :attach-by-class)
                                 before))))))
                 (list (thread-join th) tpd-global-x)))"
        ),
        // 50 sets, then the first `plist-get` and the second stats call.
        "(52 49)"
    );
}

/// Item 7(c): a thread's binding of a projected special is its own, and
/// reading it needs no attach while main reads the canonical value.
#[test]
fn a_projected_binding_reads_detached_beside_the_canonical_value() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (defvar tpd-v 1)
               (let ((th (make-thread
                          (lambda ()
                            (let ((tpd-v 2) (n 0) (ok t) (before nil))
                              (setq before (plist-get (neomacs-shared-heap-stats)
                                                      :attach-by-class))
                              (while (< n 20000)
                                (if (/= tpd-v 2) (setq ok nil))
                                (setq n (1+ n)))
                              (list ok tpd-v
                                    (- (plist-get (neomacs-shared-heap-stats)
                                                  :attach-by-class)
                                       before)))))))
                 (thread-yield)
                 (let ((n 0) (ok t))
                   (while (< n 20000)
                     (if (/= tpd-v 1) (setq ok nil))
                     (setq n (1+ n)))
                   (list ok tpd-v (thread-join th)))))"
        ),
        // The thread's loop attaches for nothing: the 2 are its first
        // `plist-get` and its second stats call.
        "(t 1 (t 2 2))"
    );
}

/// Item 7(d): buffer primitives attach and work.
#[test]
fn buffer_primitives_in_a_detached_thread_attach_and_work() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(let ((th (make-thread
                        (lambda ()
                          (let ((b (get-buffer-create \"tpd-buffer\")))
                            (set-buffer b)
                            (insert \"abc\")
                            (insert \"def\")
                            (buffer-string))))))
               (list (thread-join th)
                     (> (plist-get (neomacs-shared-heap-stats) :attach-by-class) 4)))"
        ),
        "(\"abcdef\" t)"
    );
}

/// Item 6 and 7(e): `thread-signal` stops a detached spinner at its next
/// quit poll.
#[test]
fn thread_signal_stops_a_detached_spinner() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(let ((th (make-thread
                        (lambda ()
                          (let ((n 0))
                            (while t (setq n (1+ n))))))))
               ;; The spinner attaches for its prologue and detaches.
               (thread-yield)
               (thread-signal th 'error '(\"stop\"))
               (list (condition-case err (thread-join th) (error err))
                     (thread-live-p th)))"
        ),
        "((error \"stop\") nil)"
    );
}

/// Item 5 and 7(f): a consing detached thread defers the collections it
/// makes due; the deferred one runs once no thread is detached.
#[test]
fn collections_wait_for_detached_threads_and_run_after_the_join() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(let ((th (make-thread
                        (lambda ()
                          (let ((n 0) (l nil) (g gcs-done))
                            (while (< n 300000)
                              (setq l (cons n l))
                              (setq n (1+ n)))
                            ;; No World-class primitive here: one would let
                            ;; this thread, attached and the only one
                            ;; detached, run the collection itself.
                            (list (car l) (- gcs-done g) gcs-done))))))
               (setq tpd-result (thread-join th))
               (list (car tpd-result) (car (cdr tpd-result))))"
        ),
        // No collection ran while the thread consed.
        "(299999 0)"
    );
    let deferred = stats(&mut ev);
    assert!(deferred.deferred_bytes > 0, "{deferred:?}");
    assert!(deferred.attach_by_alloc > 0, "{deferred:?}");
    // Main is the only thread now: its next safe points collect.
    assert_eq!(
        eval(
            &mut ev,
            "(let ((n 0) (seen (car (cdr (cdr tpd-result)))))
               (while (and (= gcs-done seen) (< n 100000)) (setq n (1+ n)))
               (> gcs-done seen))"
        ),
        "t"
    );
    let collected = stats(&mut ev);
    println!("parallel stats after a deferred collection: {collected:?}");
    assert!(collected.collections >= 1, "{collected:?}");
}

/// [`stats`], all zero before the first thread starts the World's lock.
fn stats_so_far(ev: &mut Context) -> ParallelStats {
    ev.parallel_lock().map(BatonLock::stats).unwrap_or_default()
}

/// The attaches detached threads made between BEFORE and AFTER, per
/// reason: (class, alloc, intern).
fn attaches_between(before: &ParallelStats, after: &ParallelStats) -> (u64, u64, u64) {
    (
        after.attach_by_class - before.attach_by_class,
        after.attach_by_alloc - before.attach_by_alloc,
        after.attach_by_intern - before.attach_by_intern,
    )
}

/// The byte-compiled `(lambda (n s) (let ((i 0)) (while (< i n) (setq s
/// (+ s (* i i))) (setq i (1+ i))) s))`: started at a bignum S, every
/// `+` makes a new bignum.
const BIGNUM_SUM: &str = r#"(make-byte-code 514 "\300\211\003\127\203\022\000\001\001\211\137\134\262\002\124\202\001\000\001\207" [0] 6)"#;

/// Two threads run the bignum loop of N iterations from S0 at once;
/// returns both results and the threads' attaches.
fn two_bignum_loops(ev: &mut Context, n: i128) -> (String, (u64, u64, u64)) {
    let before = stats_so_far(ev);
    let result = eval(
        ev,
        &format!(
            "(let ((a (make-thread (lambda () (tpd-bignum-sum {n} (* 4 most-positive-fixnum)))))
                   (b (make-thread (lambda () (tpd-bignum-sum {n} (* 4 most-positive-fixnum))))))
               (list (thread-join a) (thread-join b)))"
        ),
    );
    (result, attaches_between(&before, &stats(ev)))
}

/// A byte-compiled loop whose accumulator is a bignum allocates its
/// bignums from each thread's own runs: the attaches do not grow with the
/// iterations (one heap attach per thread makes its allocation state).
#[test]
fn a_detached_bignum_loop_attaches_a_bounded_number_of_times() {
    let mut ev = parallel();
    eval(&mut ev, &format!("(fset 'tpd-bignum-sum {BIGNUM_SUM})"));
    let s0 = 4 * ((1_i128 << 61) - 1);
    let expected = |n: i128| s0 + (n - 1) * n * (2 * n - 1) / 6;
    for n in [2_000, 20_000] {
        let (result, attaches) = two_bignum_loops(&mut ev, n);
        assert_eq!(result, format!("({0} {0})", expected(n)));
        println!(
            "bignum loop, 2 threads x {n} iterations: attaches (class, alloc, intern) {attaches:?}"
        );
        let (class, alloc, intern) = attaches;
        assert_eq!((class, intern), (0, 0), "{attaches:?}");
        assert!(alloc <= 2, "{n} iterations: {attaches:?}");
    }
}

/// About 180 KB of JSON: 1000 records of strings, integers, floats, nested
/// arrays and objects, with 8 distinct member names.
fn json_document() -> String {
    use std::fmt::Write as _;
    let mut json = String::from("[");
    for i in 0..1000 {
        if i > 0 {
            json.push(',');
        }
        write!(
            json,
            "{{\"id\":{i},\"name\":\"record-{i}-{:0>40}\",\"score\":{}.5,\
             \"tags\":[\"alpha\",\"beta\",\"gamma-{i}\"],\
             \"pos\":[{}.25,{}.75,{}.125],\"meta\":{{\"rank\":{},\"note\":\"n{i}\"}}}}",
            i * 7,
            i * 3,
            i,
            i + 1,
            i + 2,
            i % 17,
        )
        .expect("a String takes every write");
    }
    json.push(']');
    json
}

/// Bind `tpd-json` to the document, as a Lisp string literal.
fn set_json_document(ev: &mut Context) {
    let literal = json_document().replace('\\', "\\\\").replace('"', "\\\"");
    assert_eq!(
        eval(ev, &format!("(progn (setq tpd-json \"{literal}\") t)")),
        "t"
    );
}

/// `json-parse-string` builds its conses, strings, floats, vectors and hash
/// tables detached, from each thread's own runs: one call per thread on 2
/// threads attaches for its allocation state only, never for the call.
#[test]
fn a_detached_json_parse_string_attaches_a_bounded_number_of_times() {
    let mut ev = parallel();
    set_json_document(&mut ev);
    let before = stats_so_far(&mut ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((a (make-thread
                       (lambda () (json-parse-string tpd-json :object-type 'hash-table))))
                   (b (make-thread
                       (lambda () (json-parse-string tpd-json :object-type 'hash-table)))))
               (let ((ra (thread-join a)) (rb (thread-join b)))
                 (list (length ra) (gethash \"id\" (aref ra 999))
                       (gethash \"name\" (aref rb 3))
                       (gethash \"pos\" (aref ra 2))
                       (hash-table-count (aref rb 999))
                       (gethash \"rank\" (gethash \"meta\" (aref rb 20))))))"
        ),
        "(1000 999 \"record-3-0000000000000000000000000000000000000021\" [2.25 3.75 4.125] 6 3)"
    );
    let attaches = attaches_between(&before, &stats(&mut ev));
    println!("json-parse-string, 2 threads x 1 call: attaches (class, alloc, intern) {attaches:?}");
    let (class, alloc, intern) = attaches;
    assert_eq!((class, intern), (0, 0), "{attaches:?}");
    assert!(alloc <= 2, "{attaches:?}");
}

/// With `:object-type 'alist` the member names become symbols through the
/// process-wide symbol registry, which is thread-safe and holds no World
/// state: that needs no attach either, and the result is the serial parse's.
#[test]
fn a_detached_json_parse_string_to_alists_attaches_a_bounded_number_of_times() {
    let mut ev = parallel();
    set_json_document(&mut ev);
    let before = stats_so_far(&mut ev);
    assert_eq!(
        eval(
            &mut ev,
            "(let ((a (make-thread
                       (lambda () (json-parse-string tpd-json :object-type 'alist))))
                   (b (make-thread
                       (lambda () (json-parse-string tpd-json :object-type 'alist)))))
               (let ((serial (json-parse-string tpd-json :object-type 'alist)))
                 (list (equal (thread-join a) serial) (equal (thread-join b) serial)
                       (cdr (assq 'note (cdr (assq 'meta (aref serial 7))))))))"
        ),
        "(t t \"n7\")"
    );
    let attaches = attaches_between(&before, &stats(&mut ev));
    println!(
        "json-parse-string alist, 2 threads x 1 call: attaches (class, alloc, intern) {attaches:?}"
    );
    let (class, alloc, intern) = attaches;
    assert_eq!((class, intern), (0, 0), "{attaches:?}");
    assert!(alloc <= 2, "{attaches:?}");
}

/// A malformed document signals from the detached parse as from a serial
/// one: the same condition and data.
#[test]
fn a_detached_json_parse_string_signals_as_a_serial_one() {
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(let ((parse (lambda (s)
                            (condition-case err (json-parse-string s) (error err)))))
               (let ((th (make-thread (lambda ()
                                        (list (funcall parse \"[1, 2\")
                                              (funcall parse \"{\\\"a\\\": tru}\"))))))
                 (equal (thread-join th)
                        (list (funcall parse \"[1, 2\")
                              (funcall parse \"{\\\"a\\\": tru}\")))))"
        ),
        "t"
    );
}

/// Item 8's measurement in a test binary: the numeric loop on N threads
/// versus the same loops one after another on main, with each thread's
/// binding conses allocated next to each other's and, padded, apart.
/// Prints; the only assertions are on values.
#[test]
#[ignore = "a measurement: run with --run-ignored only"]
fn measure_the_numeric_loop_serial_versus_parallel() {
    const ITERATIONS: i64 = 1_000_000;
    // `NEOVM_MEASURE_THREADS` (the pod's `nproc`), else every CPU the
    // process may use.
    let threads = std::env::var("NEOVM_MEASURE_THREADS")
        .ok()
        .and_then(|threads| threads.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    let mut ev = parallel();
    let expected = numeric_loop(ITERATIONS);
    let all = format!("({})", vec![expected.to_string(); threads].join(" "));
    println!(
        "| nproc | binding conses | serial (main, {threads} loops) | parallel ({threads} threads) | speedup |"
    );
    for (padding, label) in [(0, "adjacent"), (4096, "padded apart")] {
        eval(
            &mut ev,
            &format!(
                "(fset 'tpd-loop
                       (lambda ()
                         (let ((pad (make-list {padding} nil)))
                           (let ((s 0) (i 0))
                             (while (< i {ITERATIONS})
                               (setq s (logand (+ s (* i i)) #xffffff))
                               (setq i (1+ i)))
                             s))))"
            ),
        );
        let serial_form = format!(
            "(let ((k 0) (r nil))
               (while (< k {threads}) (setq r (cons (tpd-loop) r)) (setq k (1+ k)))
               r)"
        );
        let parallel_form = format!(
            "(let ((k 0) (ts nil) (r nil))
               (while (< k {threads}) (setq ts (cons (make-thread 'tpd-loop) ts)) (setq k (1+ k)))
               (while ts (setq r (cons (thread-join (car ts)) r)) (setq ts (cdr ts)))
               r)"
        );
        let started = std::time::Instant::now();
        assert_eq!(eval(&mut ev, &serial_form), all);
        let serial = started.elapsed();
        let started = std::time::Instant::now();
        assert_eq!(eval(&mut ev, &parallel_form), all);
        let parallel = started.elapsed();
        println!(
            "| {threads} | {label} | {serial:.2?} | {parallel:.2?} | {:.2} |",
            serial.as_secs_f64() / parallel.as_secs_f64()
        );
    }
    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "loadavg: {}stats: {}",
        loadavg,
        eval(&mut ev, "(neomacs-shared-heap-stats)")
    );
}
