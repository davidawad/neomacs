//! An explicit collection's sweep on threads (`parallel_sweep.rs`) leaves the
//! heap as the serial sweep leaves it.
use super::*;
use crate::heap_types::LispString;
use std::sync::atomic::Ordering;

/// What a swept heap holds, to compare between a serial and a threaded sweep.
#[derive(Debug, PartialEq, Eq)]
struct Swept {
    live_bytes: usize,
    cons_live: usize,
    pages: [usize; 6],
    survivors: usize,
    garbage_owned: usize,
}

/// A heap with floats, strings, vectors, records, closures, bignums and
/// conses, one in seven rooted and the rest garbage; collected exactly with
/// SWEEP_THREADS threads. Returns what the sweep left and the survivors.
fn collected(generational: bool, sweep_threads: usize) -> (Swept, TaggedHeap, Vec<TaggedValue>) {
    let mut heap = TaggedHeap::new();
    if generational {
        // SAFETY: serial fixture heap; the caller holds the only heap view and
        // no collector or compiled leaf runs.
        unsafe { heap.set_generational_enabled(true) };
    }
    heap.partition_dump = false;
    heap.dump_blackened = false;
    heap.set_gc_threshold(usize::MAX);
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for i in 0..140_000_i64 {
        let value = match i % 8 {
            0 => heap.alloc_string(LispString::new(format!("string {i}"), false)),
            1 => heap.alloc_vector(vec![TaggedValue::fixnum(i); 3]),
            2 => heap.alloc_float(i as f64),
            3 => heap.alloc_bignum(Integer::from(i) << 70_u64),
            4 => heap.alloc_record(vec![TaggedValue::fixnum(i), TaggedValue::NIL]),
            5 => heap.alloc_lambda(vec![TaggedValue::NIL; 4]),
            _ => heap.alloc_cons(TaggedValue::fixnum(i), TaggedValue::NIL),
        };
        if i % 7 == 0 {
            live.push(value);
        } else {
            dead.push(value);
        }
    }
    knobs::with_sweep_threads_for_test(sweep_threads, || heap.collect_exact(live.iter().copied()));
    let swept = Swept {
        live_bytes: heap.live_bytes,
        cons_live: heap.cons_live_count_exact(),
        pages: [
            heap.float_arena.pages.len(),
            heap.string_arena.pages.len(),
            heap.vector_arena.pages.len(),
            heap.record_arena.pages.len(),
            heap.lambda_arena.pages.len(),
            heap.bignum_arena.pages.len(),
        ],
        survivors: live
            .iter()
            .filter(|&&value| heap.owns_heap_value_for_test(value))
            .count(),
        garbage_owned: dead
            .iter()
            .filter(|&&value| heap.owns_heap_value_for_test(value))
            .count(),
    };
    (swept, heap, live)
}

fn threads_leave_what_one_thread_leaves(generational: bool) {
    let (serial, mut serial_heap, _) = collected(generational, 1);
    let before = parallel_sweep::SPAWNED_SHARES.load(Ordering::Relaxed);
    let (spread, mut spread_heap, live) = collected(generational, 4);
    assert!(
        parallel_sweep::SPAWNED_SHARES.load(Ordering::Relaxed) > before,
        "the sweep ran on threads"
    );
    assert_eq!(spread, serial);
    assert_eq!(spread.garbage_owned, 0);
    assert_eq!(spread.survivors, live.len());
    for (index, &value) in live.iter().enumerate() {
        let i = i64::try_from(index * 7).unwrap();
        match i % 8 {
            0 => assert_eq!(
                value.as_str_owned().as_deref(),
                Some(&*format!("string {i}"))
            ),
            1 => assert_eq!(value.as_vector_data().unwrap()[2], TaggedValue::fixnum(i)),
            2 => assert_eq!(value.xfloat(), i as f64),
            6 | 7 => assert_eq!(value.cons_car(), TaggedValue::fixnum(i)),
            _ => {}
        }
    }
    // The freed slots are on the partial chains: refilling them takes no new
    // page and no new block, in either heap.
    for heap in [&mut serial_heap, &mut spread_heap] {
        let pages = (heap.string_arena.pages.len(), heap.vector_arena.pages.len());
        let blocks = heap.cons_blocks.len();
        for i in 0..1000 {
            heap.alloc_string(LispString::new(format!("again {i}"), false));
            heap.alloc_vector(vec![TaggedValue::NIL]);
            heap.alloc_cons(TaggedValue::NIL, TaggedValue::NIL);
        }
        assert_eq!(
            (heap.string_arena.pages.len(), heap.vector_arena.pages.len()),
            pages
        );
        assert_eq!(heap.cons_blocks.len(), blocks);
    }
}

#[test]
fn a_sweep_on_threads_leaves_what_one_thread_leaves() {
    threads_leave_what_one_thread_leaves(false);
}

#[test]
fn a_generational_sweep_on_threads_leaves_what_one_thread_leaves() {
    threads_leave_what_one_thread_leaves(true);
}
