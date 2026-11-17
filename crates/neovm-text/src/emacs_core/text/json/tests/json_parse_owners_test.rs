//! Shared-heap experiment, per-owner allocation: `json-parse-string` on N OS
//! threads at once, each allocating from its own runs, against the same
//! parses on one thread. Counts are asserted; times are printed only.

use super::*;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

/// Records in the document: about 200 KB of JSON.
const RECORDS: usize = 1_000;
/// Parses per thread.
const PARSES: usize = 20;

/// A deterministic document of `RECORDS` objects with strings, integers,
/// floats and nested arrays: what a parse allocates is strings, hash tables,
/// vectors and floats.
fn document() -> String {
    let mut json = String::from("[");
    for i in 0..RECORDS {
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
        .unwrap();
    }
    json.push(']');
    json
}

/// Parse `json` with `:object-type 'hash-table` and check its shape.
fn parse_and_check(json: Value) {
    let parsed = builtin_json_parse_string(vec![
        json,
        Value::keyword(":object-type"),
        Value::symbol("hash-table"),
    ])
    .expect("the document parses");
    let records = parsed.as_vector_data().expect("a vector of records");
    assert_eq!(records.len(), RECORDS);
    let last = records[RECORDS - 1]
        .as_hash_table()
        .expect("a record table");
    assert_eq!(last.data.len(), 6);
}

/// `NEOVM_OWNER_ALLOC_THREADS`, else the available parallelism.
fn threads() -> usize {
    std::env::var("NEOVM_OWNER_ALLOC_THREADS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(2, usize::from))
}

#[test]
fn json_parse_string_runs_on_owner_threads_at_once() {
    crate::test_utils::init_test_tracing();
    let text = document();
    let bytes = text.len();
    let json = Value::string(text);
    let n = threads();

    // Serial: the keeper alone parses what all N threads parse below.
    let serial_start = Instant::now();
    for _ in 0..n * PARSES {
        parse_and_check(json);
    }
    let serial = serial_start.elapsed();

    // Parallel: N owner threads, PARSES each, through the owner allocator.
    let done = AtomicUsize::new(0);
    let input = json.bits();
    let parallel_start = Instant::now();
    let (refills, lock_wait_ns) = neovm_value::tagged::gc::run_detached_owners_for_test(n, |_| {
        // SAFETY: the keeper's input string, live and unmodified while the
        // section runs.
        let json = unsafe { Value::from_bits(input) };
        for _ in 0..PARSES {
            parse_and_check(json);
            done.fetch_add(1, Ordering::Relaxed);
        }
    });
    let parallel = parallel_start.elapsed();
    assert_eq!(done.load(Ordering::Relaxed), n * PARSES);
    assert!(refills > 0);

    let loadavg = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    println!(
        "owner-alloc json-parse-string: threads={n} parses/thread={PARSES} \
         doc_bytes={bytes} serial_ms={} parallel_ms={} speedup={:.2} \
         refills={refills} lock_wait_ns={lock_wait_ns} loadavg={}",
        serial.as_millis(),
        parallel.as_millis(),
        serial.as_secs_f64() / parallel.as_secs_f64(),
        loadavg.trim(),
    );
}
