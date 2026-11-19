//! A Parallel World's in-stop sweep (`sweep_in_stop_on`): spread over
//! threads, it leaves the heap exactly as the serial slices do.
use super::*;
use crate::heap_types::LispString;
use crate::value::{HashTableTest, LispHashTable};
use std::num::NonZeroUsize;

/// A heap with every class the in-stop sweep spreads, a seventh of each
/// rooted, the rest garbage, after a minor collection's mark: its sweep is
/// deferred. Returns the heap, the roots and the garbage.
fn heap_in_its_sweep() -> (TaggedHeap, Vec<TaggedValue>, Vec<TaggedValue>) {
    let mut heap = TaggedHeap::new();
    // SAFETY: serial fixture heap; the caller holds the only heap view and
    // no collector or compiled leaf runs.
    unsafe { heap.set_generational_enabled(true) };
    heap.partition_dump = false;
    heap.dump_blackened = false;
    heap.set_gc_threshold(usize::MAX);
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for i in 0..60_000_i64 {
        let value = match i % 6 {
            0 | 1 => heap.alloc_string(LispString::new(format!("string {i}"), false)),
            2 => heap.alloc_vector(vec![TaggedValue::fixnum(i)]),
            3 => heap.alloc_float(i as f64),
            4 => heap.alloc_bignum(Integer::from(i) << 70_u64),
            _ => heap.alloc_hash_table(LispHashTable::new(HashTableTest::Eq)),
        };
        if i % 7 == 0 {
            live.push(value);
        } else {
            dead.push(value);
        }
    }
    heap.begin_minor_collection().unwrap();
    for &root in &live {
        heap.seed_root(root);
    }
    heap.complete_minor_collection();
    assert!(heap.sweep_in_progress());
    (heap, live, dead)
}

/// What a finished sweep left, to compare.
fn swept_state(heap: &TaggedHeap) -> [usize; 9] {
    [
        heap.live_bytes,
        heap.sweep_lifetime_noncons_freed,
        heap.non_cons_object_addrs.len(),
        heap.vector_object_addrs.len(),
        heap.current_mutator_gc().allocated_count,
        heap.float_arena.pages.len(),
        heap.string_arena.pages.len(),
        heap.vector_arena.pages.len(),
        heap.bignum_arena.pages.len(),
    ]
}

#[test]
fn the_in_stop_sweep_on_four_threads_leaves_what_the_slices_leave() {
    let (mut serial, serial_live, serial_dead) = heap_in_its_sweep();
    serial.finish_incremental_sweep_now();

    let (mut spread, live, dead) = heap_in_its_sweep();
    spread.sweep_in_stop_on(NonZeroUsize::new(4).unwrap(), |parts, share| {
        std::thread::scope(|threads| {
            for index in 1..parts {
                threads.spawn(move || share(index));
            }
            share(0);
        });
    });
    assert!(
        spread.sweep_noncons_pending.is_null()
            && spread.sweep_float_page_cursor == spread.sweep_float_page_end
            && spread.sweep_string_page_cursor == spread.sweep_string_page_end
            && spread.sweep_vector_page_cursor == spread.sweep_vector_page_end
            && spread.sweep_bignum_page_cursor == spread.sweep_bignum_page_end,
        "the in-stop sweep leaves the slices no page of these classes and no young object"
    );
    spread.finish_incremental_sweep_now();

    assert_eq!(swept_state(&spread), swept_state(&serial));
    for (heap, live, dead) in [
        (&serial, &serial_live, &serial_dead),
        (&spread, &live, &dead),
    ] {
        assert!(
            live.iter()
                .all(|&value| heap.owns_heap_value_for_test(value))
        );
        assert!(
            !dead
                .iter()
                .any(|&value| heap.owns_heap_value_for_test(value))
        );
    }
    for (index, &value) in live.iter().enumerate() {
        let i = i64::try_from(index * 7).unwrap();
        match i % 6 {
            0 | 1 => assert_eq!(
                value.as_str_owned().as_deref(),
                Some(&*format!("string {i}"))
            ),
            2 => assert_eq!(value.as_vector_data().unwrap()[0], TaggedValue::fixnum(i)),
            3 => assert_eq!(value.xfloat(), i as f64),
            _ => {}
        }
    }

    // The freed slots are on the partial chains: refilling them takes no
    // new page, in either heap.
    for heap in [&mut serial, &mut spread] {
        let pages = swept_state(heap)[5..].to_vec();
        for i in 0..1000 {
            heap.alloc_string(LispString::new(format!("again {i}"), false));
            heap.alloc_vector(vec![TaggedValue::NIL]);
        }
        assert_eq!(swept_state(heap)[5..], pages[..]);
    }
}

/// Run on owner threads, the in-stop sweep frees no dead payload while the
/// World is stopped: each share hands the dead strings', vectors' and
/// bignums' payloads and the dead boxes to the owner state of the thread
/// that ran it, which frees them after the stop. The heap is left as the
/// slices leave it.
#[test]
fn the_in_stop_sweep_leaves_dead_payloads_to_the_threads_that_swept_them() {
    let (mut serial, _, _) = heap_in_its_sweep();
    serial.finish_incremental_sweep_now();

    let (mut spread, live, dead) = heap_in_its_sweep();
    // Every dead object but the floats owns allocator memory.
    let payloads = (0..60_000_i64).filter(|i| i % 7 != 0 && i % 6 != 3).count();
    let mut states: Vec<_> = (0..4).map(|_| spread.new_owner_alloc_state()).collect();
    spread.sweep_in_stop_on(NonZeroUsize::new(4).unwrap(), |parts, share| {
        std::thread::scope(|threads| {
            for (index, state) in states.iter_mut().enumerate().take(parts) {
                threads.spawn(move || {
                    let _installed = state.install();
                    share(index);
                });
            }
        });
    });
    let held: usize = states.iter().map(|state| state.swept_payload_count()).sum();
    assert_eq!(held, payloads, "every dead payload waits for its thread");
    assert!(
        states
            .iter()
            .filter(|state| state.swept_payload_count() > 0)
            .count()
            > 1,
        "the shares ran on several owner threads"
    );
    spread.finish_incremental_sweep_now();
    assert_eq!(swept_state(&spread), swept_state(&serial));
    assert!(
        live.iter()
            .all(|&value| spread.owns_heap_value_for_test(value))
    );
    assert!(
        !dead
            .iter()
            .any(|&value| spread.owns_heap_value_for_test(value))
    );
    for state in &mut states {
        state.free_swept_payloads();
        assert_eq!(state.swept_payload_count(), 0);
    }
}
