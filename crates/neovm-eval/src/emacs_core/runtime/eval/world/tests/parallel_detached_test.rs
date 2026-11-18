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

/// GNU `dotimes` (subr.el), as a macro built without backquote: the
/// unit Context loads no Lisp files.
const DOTIMES: &str = "(defalias 'tpd-dotimes
       (cons 'macro
             (lambda (spec &rest body)
               (let ((upper (make-symbol \"upper\"))
                     (counter (make-symbol \"counter\")))
                 (list 'let (list (list upper (nth 1 spec)) (list counter 0))
                       (list 'while (list '< counter upper)
                             (cons 'let (cons (list (list (car spec) counter)) body))
                             (list 'setq counter (list '1+ counter))))))))";

/// Attaches of two Parallel threads that each run the never-compiled
/// `tpd-interp-loop` for ITERATIONS: one `let` of a lexical local per
/// iteration (the `dotimes` expansion), `setq` of lexical locals and
/// fixnum arithmetic. Returns the growth of the attach counters.
fn interpreted_loop_attaches(ev: &mut Context, iterations: i64) -> ParallelStats {
    // The lock exists once the first thread started the Parallel runtime.
    let before = ev.parallel_lock().map(BatonLock::stats).unwrap_or_default();
    let serial = numeric_loop(iterations);
    assert_eq!(
        eval(
            ev,
            &format!(
                "(let ((a (make-thread (lambda () (tpd-interp-loop {iterations}))))
                       (b (make-thread (lambda () (tpd-interp-loop {iterations})))))
                   (list (thread-join a) (thread-join b)))"
            )
        ),
        format!("({serial} {serial})")
    );
    let after = stats(ev);
    ParallelStats {
        attach_count: after.attach_count - before.attach_count,
        attach_by_class: after.attach_by_class - before.attach_by_class,
        attach_by_alloc: after.attach_by_alloc - before.attach_by_alloc,
        attach_by_intern: after.attach_by_intern - before.attach_by_intern,
        attach_by_blv: after.attach_by_blv - before.attach_by_blv,
        requester_parks: after.requester_parks - before.requester_parks,
        ..after
    }
}

/// An interpreted fixnum loop on two threads attaches O(1) times, not
/// O(iterations): its lexical bindings, `setq`s and arithmetic run
/// detached as the byte-compiled loop's do.
#[test]
fn an_interpreted_dotimes_loop_attaches_a_constant_number_of_times() {
    let mut ev = parallel();
    assert_eq!(eval(&mut ev, DOTIMES), "tpd-dotimes");
    eval(
        &mut ev,
        "(fset 'tpd-interp-loop
               (lambda (n)
                 (let ((s 0))
                   (tpd-dotimes (i n)
                     (setq s (logand (+ s (* i i)) #xffffff)))
                   s)))",
    );
    let small = interpreted_loop_attaches(&mut ev, 1_000);
    let large = interpreted_loop_attaches(&mut ev, 10_000);
    println!("attaches for 2 x 1000 iterations: {small:?}");
    println!("attaches for 2 x 10000 iterations: {large:?}");
    // Thread start, join and end attach a fixed number of times; one
    // attach per iteration would be at least 20000 here. A collection's
    // requester that waits for the threads to reach a safe point attaches
    // once per park, as many times as the wait lasts: those attaches count
    // how slow the build is, not the loop's work, and are left out.
    let work = |stats: &ParallelStats| stats.attach_count - stats.requester_parks;
    assert!(
        work(&large) < 64,
        "the interpreted loop attaches per iteration: {large:?}"
    );
    assert!(
        work(&large) <= work(&small) + 16,
        "attaches grow with the iteration count: {small:?} then {large:?}"
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

/// One `bench.el` workload: the function a thread calls and its argument.
struct BenchWorkload<'src> {
    name: &'src str,
    /// The `fset` form that defines the function.
    define: &'src str,
    /// The form each thread evaluates.
    call: &'src str,
}

/// `bench.el`'s functions, byte-compiled by GNU Emacs (`b-` names, the
/// `make-byte-code` of each `byte-compile`) or interpreted (`i-fix`).
/// The sizes are the benchmark's divided
/// down for a test build; the JSON document is [`bench_json_document`].
const BENCH_WORKLOADS: [BenchWorkload<'static>; 6] = [
    BenchWorkload {
        name: "bytecode-fixnum-loop",
        define: r#"(fset 'b-fix (make-byte-code 257 "\300\211\211\3W\203\26\0\301\2\2\302_\\\303\"\262\2T\202\2\0\210\207" [0 logand 7 1048575] 7))"#,
        call: "(b-fix 2000000)",
    },
    BenchWorkload {
        name: "bytecode-cons-loop",
        define: r#"(fset 'b-cons (make-byte-code 257 "\300\301\211\3W\203\31\0\211\2G\302V?\205\22\0\2B\262\2T\202\2\0\210G\207" [nil 0 50] 6))"#,
        call: "(b-cons 40000)",
    },
    // `b-big`'s sum leaves the fixnum range after about 1.9M iterations;
    // started at a bignum ([`BIGNUM_SUM`]), every `+` allocates one.
    BenchWorkload {
        name: "bytecode-bignum-loop",
        define: r#"(fset 'b-big (make-byte-code 514 "\300\211\003\127\203\022\000\001\001\211\137\134\262\002\124\202\001\000\001\207" [0] 6))"#,
        call: "(logand (b-big 30000 (* 4 most-positive-fixnum)) 1048575)",
    },
    BenchWorkload {
        name: "json-parse-hash",
        define: r#"(fset 'b-jh (make-byte-code 257 "\301\302\10\303\304#\305H!\207" [b-json hash-table-count json-parse-string :object-type hash-table 0] 6))"#,
        call: "(b-jh nil)",
    },
    BenchWorkload {
        name: "json-parse-alist",
        define: r#"(fset 'b-ja (make-byte-code 257 "\301\10\302\303\304\305%G\207" [b-json json-parse-string :object-type alist :array-type list] 7))"#,
        call: "(b-ja nil)",
    },
    BenchWorkload {
        name: "interpreted-fixnum-loop",
        // `(dotimes (i n) ...)` as GNU's `dotimes` (subr.el) expands it:
        // the editor expands macros eagerly when it loads `bench.el`.
        define: "(fset 'i-fix
                   (lambda (n)
                     (let ((s 0))
                       (let ((upper n) (counter 0))
                         (while (< counter upper)
                           (let ((i counter))
                             (setq s (logand (+ s (* i 7)) 1048575)))
                           (setq counter (1+ counter))))
                       s)))",
        call: "(i-fix 20000)",
    },
];

/// `bench.el`'s `b-json`: an array of RECORDS objects of four members.
fn bench_json_document(records: usize) -> String {
    let objects: Vec<String> = (1..=records)
        .map(|i| format!(r#"{{"id":{i},"name":"item-{i}","tags":["a","b","c"],"v":{i}.5}}"#))
        .collect();
    format!("[{}]", objects.join(","))
}

/// The growth of every counter from BEFORE to AFTER (`threads` is AFTER's).
fn stats_delta(before: &ParallelStats, after: &ParallelStats) -> ParallelStats {
    ParallelStats {
        threads: after.threads,
        attach_count: after.attach_count - before.attach_count,
        attach_wait_ns: after.attach_wait_ns - before.attach_wait_ns,
        detached_ns: after.detached_ns - before.detached_ns,
        safepoint_stops: after.safepoint_stops - before.safepoint_stops,
        collections: after.collections - before.collections,
        deferred_bytes: after.deferred_bytes - before.deferred_bytes,
        attach_by_class: after.attach_by_class - before.attach_by_class,
        attach_by_alloc: after.attach_by_alloc - before.attach_by_alloc,
        attach_by_intern: after.attach_by_intern - before.attach_by_intern,
        attach_by_blv: after.attach_by_blv - before.attach_by_blv,
        stop_wait_ns: after.stop_wait_ns - before.stop_wait_ns,
        stop_max_ns: after.stop_max_ns.max(before.stop_max_ns),
        requester_parks: after.requester_parks - before.requester_parks,
    }
}

/// A Parallel World with every [`BENCH_WORKLOADS`] function defined.
fn bench_world() -> Context {
    let mut ev = parallel();
    let literal = bench_json_document(1000).replace('"', "\\\"");
    assert_eq!(
        eval(&mut ev, &format!("(progn (setq b-json \"{literal}\") t)")),
        "t"
    );
    for workload in &BENCH_WORKLOADS {
        eval(&mut ev, workload.define);
    }
    ev
}

/// Run WORKLOAD on THREADS Lisp threads at once, check every result
/// against SERIAL (a serial call's), and return the counters the run
/// added: the threads' attaches and the collections main runs once they
/// are joined.
fn bench_row(
    ev: &mut Context,
    workload: &BenchWorkload<'_>,
    threads: usize,
    serial: &str,
) -> ParallelStats {
    let before = stats_so_far(ev);
    let form = format!(
        "(let ((k 0) (ts nil))
           (while (< k {threads})
             (setq ts (cons (make-thread (lambda () {call})) ts))
             (setq k (1+ k)))
           (let ((results (mapcar 'thread-join ts)) (n 0))
             ;; Main's safe points after the joins finish any sweep left.
             (while (< n 2000) (setq n (1+ n)))
             results))",
        call = workload.call
    );
    let expected = format!("({})", vec![serial; threads].join(" "));
    assert_eq!(eval(ev, &form), expected, "{} x {threads}", workload.name);
    stats_delta(&before, &stats(ev))
}

/// Attribution of the `bench.el` workloads' parallel costs: each one on 2,
/// 4 and 8 Lisp threads, with the counters `neomacs-shared-heap-stats`
/// reports. Every thread's result equals the serial call's; the table is
/// printed for VERIFY.md (the nanosecond columns are measurements, never
/// asserted).
#[test]
fn bench_workloads_on_2_4_8_threads_report_their_attaches_and_collections() {
    let mut ev = bench_world();
    println!(
        "| workload | threads | attach-count | by-alloc | by-class | by-intern | by-blv \
         | attach-wait-ns | collections | safepoint-stops | deferred-bytes | stop-wait-ns |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for workload in &BENCH_WORKLOADS {
        let serial = eval(&mut ev, workload.call);
        for threads in [2, 4, 8] {
            let row = bench_row(&mut ev, workload, threads, &serial);
            println!(
                "| {} | {threads} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                workload.name,
                row.attach_count,
                row.attach_by_alloc,
                row.attach_by_class,
                row.attach_by_intern,
                row.attach_by_blv,
                row.attach_wait_ns,
                row.collections,
                row.safepoint_stops,
                row.deferred_bytes,
                row.stop_wait_ns,
            );
        }
    }
}

/// [`bench_workloads_on_2_4_8_threads_report_their_attaches_and_collections`]
/// as wall-clock times: serial (main, one call per thread, its collections
/// inline) against the threads' phase (spawn to the last join, as
/// `bench.el` times it), and apart from both the collection main runs
/// after the joins (what the threads left to sweep). Each call repeats
/// `NEOVM_MEASURE_REPEAT` times. A measurement in a test build: printed
/// only.
#[test]
#[ignore = "a measurement: run with --run-ignored only"]
fn measure_bench_workloads_serial_versus_parallel() {
    let setting = |name: &str, default: usize| {
        std::env::var(name)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let threads = setting("NEOVM_MEASURE_THREADS", 8);
    let repeat = setting("NEOVM_MEASURE_REPEAT", 10);
    let mut ev = bench_world();
    // Main compiles as the threads do, as in the editor.
    #[cfg(feature = "jit")]
    crate::emacs_core::jit::force_jit_off_for_test(false);
    println!("| workload | threads | serial | threads' phase | speedup | after the joins |");
    println!("|---|---:|---:|---:|---:|---:|");
    let mut phases = Vec::new();
    for workload in &BENCH_WORKLOADS {
        let call = format!(
            "(let ((j 0) (r nil)) (while (< j {repeat}) (setq r {}) (setq j (1+ j))) r)",
            workload.call
        );
        let result = eval(&mut ev, &call);
        let serial_form = format!(
            "(let ((k 0) (r nil)) (while (< k {threads}) (setq r (cons {call} r)) (setq k (1+ k))) r)"
        );
        let parallel_form = format!(
            "(let ((k 0) (ts nil))
               (while (< k {threads})
                 (setq ts (cons (make-thread (lambda () {call})) ts))
                 (setq k (1+ k)))
               (mapcar 'thread-join ts))"
        );
        let settle = "(let ((n 0)) (while (< n 20000) (setq n (1+ n))))";
        eval(&mut ev, settle);
        let started = std::time::Instant::now();
        eval(&mut ev, &serial_form);
        let serial = started.elapsed();
        eval(&mut ev, settle);
        let before = stats_so_far(&mut ev);
        let started = std::time::Instant::now();
        let joined = eval(&mut ev, &parallel_form);
        let parallel = started.elapsed();
        phases.push((workload.name, stats_delta(&before, &stats(&mut ev))));
        let started = std::time::Instant::now();
        eval(&mut ev, settle);
        let after = started.elapsed();
        assert_eq!(
            joined,
            format!("({})", vec![result.as_str(); threads].join(" ")),
            "{}",
            workload.name
        );
        println!(
            "| {} | {threads} | {serial:.2?} | {parallel:.2?} | {:.2}x | {after:.2?} |",
            workload.name,
            serial.as_secs_f64() / parallel.as_secs_f64()
        );
    }
    // The counters of each threads' phase: what serialized it, counted.
    println!(
        "| workload | attach-count | by-alloc | by-class | by-intern | requester-parks \
         | collections | safepoint-stops | deferred-bytes | attach-wait-ns | stop-wait-ns |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for (name, row) in phases {
        println!(
            "| {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            row.attach_count,
            row.attach_by_alloc,
            row.attach_by_class,
            row.attach_by_intern,
            row.requester_parks,
            row.collections,
            row.safepoint_stops,
            row.deferred_bytes,
            row.attach_wait_ns,
            row.stop_wait_ns,
        );
    }
}

/// The bytecode VM's `cons` on a detached thread takes the thread's own
/// allocation runs: two threads interpreting `b-cons` (40000 conses each)
/// attach for their allocation state once each, not once per cons.
#[test]
fn a_bytecode_cons_loop_attaches_a_bounded_number_of_times() {
    // Every thread interprets the bytecode: no compile replaces `Op::Cons`.
    // SAFETY: before any thread of this test process reads the variable.
    unsafe { std::env::set_var("NEOVM_JIT", "0") };
    let mut ev = bench_world();
    let workload = &BENCH_WORKLOADS[1];
    let serial = eval(&mut ev, workload.call);
    let row = bench_row(&mut ev, workload, 2, &serial);
    println!("b-cons, 2 threads, JIT off: {row:?}");
    assert_eq!(row.attach_by_class, 0, "{row:?}");
    assert!(row.attach_by_alloc <= 2, "{row:?}");
}

/// Detached threads let-bind buffer-local variables of the buffer they all
/// share, one with a local binding there and one without, and read them
/// inside the binding. Each bind and unbind looks the variable up in the
/// buffer's local bindings; no thread may write that shared buffer's lazy
/// index (`LocalVariableBindings`), and no binding attaches. Run with
/// `--stress-count`: the race it guards was a `RefCell already borrowed`
/// panic on whichever thread lost it.
#[test]
fn detached_threads_bind_buffer_locals_of_a_shared_buffer_without_attaching() {
    const THREADS: usize = 4;
    const ITERATIONS: i64 = 20_000;
    let mut ev = parallel();
    assert_eq!(
        eval(
            &mut ev,
            "(progn
               (defvar tpd-blv-local 0)
               (make-variable-buffer-local 'tpd-blv-local)
               (setq tpd-blv-local 5)
               (defvar tpd-blv-default 7)
               (make-variable-buffer-local 'tpd-blv-default)
               (list (local-variable-p 'tpd-blv-local)
                     (local-variable-p 'tpd-blv-default)))"
        ),
        "(t nil)"
    );
    let before = stats_so_far(&mut ev);
    let form = format!(
        "(let ((k 0) (ts nil))
           (while (< k {THREADS})
             (setq ts
                   (cons (make-thread
                          (lambda ()
                            (let ((i 0) (s 0))
                              (while (< i {ITERATIONS})
                                (let ((tpd-blv-local i) (tpd-blv-default 1))
                                  (setq s (+ s tpd-blv-local tpd-blv-default)))
                                (setq i (1+ i)))
                              (list s tpd-blv-local tpd-blv-default))))
                         ts))
             (setq k (1+ k)))
           (list (mapcar 'thread-join ts) tpd-blv-local tpd-blv-default))"
    );
    let sum = ITERATIONS * (ITERATIONS - 1) / 2 + ITERATIONS;
    let each = format!("({sum} 5 7)");
    assert_eq!(
        eval(&mut ev, &form),
        format!("(({}) 5 7)", vec![each.as_str(); THREADS].join(" "))
    );
    let row = stats_delta(&before, &stats(&mut ev));
    println!("{THREADS} threads x {ITERATIONS} buffer-local bindings: {row:?}");
    // Thread start, join and end attach a fixed number of times; one attach
    // per binding would be at least 4 x 20000 here.
    assert!(row.attach_count < 64, "the bindings attach: {row:?}");
}

/// A compiled call of a compiled `&rest` callee on a detached thread conses
/// the rest list from the thread's own allocation runs. The callee takes
/// 16 arguments and a rest list, more than the spec shim's fast path
/// frames, so every call marshals through `Vm::run_leaf_native_to_native`:
/// two threads whose byte-compiled loop makes 2000 such calls each attach
/// for their allocation state once each, not once per call.
#[cfg(feature = "jit")]
#[test]
fn a_compiled_rest_call_on_detached_threads_attaches_a_bounded_number_of_times() {
    use crate::emacs_core::bytecode::ByteCodeFunctionExt;
    use crate::emacs_core::bytecode::value_api::TaggedValueBytecodeExt;
    use crate::emacs_core::jit::RuntimeStateTieringExt;
    const CALLS: i64 = 2_000;
    let mut ev = parallel();
    // `(lambda (a1 ... a16 &rest r) r)` and `(lambda (n) (let ((i 0) (s 0))
    // (while (< i n) (setq s (+ s (car (cdr (tpd-rest-callee i 1 ... 17)))))
    // (setq i (1+ i))) s))`, as GNU's byte compiler emits them.
    eval(
        &mut ev,
        r#"(progn
             (fset 'tpd-rest-callee (make-byte-code 4240 "\207" [] 18))
             (fset 'tpd-rest-loop
                   (make-byte-code 257 "\300\211\001\003\127\203\052\000\211\301\003\302\303\304\305\306\307\310\311\312\313\314\315\316\317\320\321\322\046\022\101\100\134\262\001\001\124\262\002\202\002\000\207"
                                   [0 tpd-rest-callee 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17] 23)))"#,
    );
    assert_eq!(eval(&mut ev, "(tpd-rest-loop 10)"), "170");
    // Each thread compiles both at its first call of them.
    for name in ["tpd-rest-callee", "tpd-rest-loop"] {
        let function = ev
            .eval_str(&format!("(symbol-function '{name})"))
            .expect("defined");
        function
            .get_bytecode_data()
            .expect("byte-code")
            .jit_runtime()
            .set_hot_for_test();
    }
    let before = stats_so_far(&mut ev);
    let thread = format!("(make-thread (lambda () (tpd-rest-loop {CALLS})))");
    assert_eq!(
        eval(
            &mut ev,
            &format!("(let ((a {thread}) (b {thread})) (list (thread-join a) (thread-join b)))")
        ),
        format!("({0} {0})", 17 * CALLS)
    );
    let row = stats_delta(&before, &stats(&mut ev));
    println!("2 threads x {CALLS} compiled &rest calls: {row:?}");
    assert!(row.attach_by_alloc <= 2, "{row:?}");
}
