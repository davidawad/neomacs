//! Per-owner allocation (`owner_pages.rs`): owner threads allocate through
//! the ordinary `Value` constructors at the same time, the keeper adopts and
//! reads everything back, and a collection between close and reopen reclaims
//! the owners' garbage.
//!
//! The detached section here is the test's: the keeper holds the heap, each
//! owner thread installs it and its owner state under a stand-in for the
//! Baton (a mutex), then releases it and allocates while the keeper waits.

use super::owner_pages::{
    OwnerAllocState, OwnerKey, OwnerPages, OwnerRunSizes, close_owner_regions, current_owner_key,
    reopen_owner_regions,
};
use super::*;
use crate::heap_types::LispString;
use crate::value::{HashKey, HashTableTest, Value};
use std::ptr::NonNull;
use std::sync::{Barrier, Mutex};

/// What one owner allocated, as raw bits (a `Value` is thread-confined).
#[derive(Default)]
struct Allocated {
    list: usize,
    strings: Vec<usize>,
    vectors: Vec<usize>,
    tables: Vec<usize>,
    floats: Vec<usize>,
}

#[derive(Clone, Copy)]
struct Counts {
    conses: usize,
    strings: usize,
    vectors: usize,
    vector_len: usize,
    tables: usize,
    table_entries: usize,
    floats: usize,
}

const FULL: Counts = Counts {
    conses: 100_000,
    strings: 10_000,
    vectors: 1_000,
    vector_len: 100,
    tables: 100,
    table_entries: 1_000,
    floats: 50_000,
};

fn value(bits: usize) -> Value {
    // SAFETY: bits of a live object this test allocated and keeps reachable.
    unsafe { Value::from_bits(bits) }
}

fn key(i: usize) -> String {
    format!("k{i}")
}

/// Allocate `counts` through the ordinary entry points; `tag` makes every
/// owner's contents distinct.
fn allocate(tag: usize, counts: Counts) -> Allocated {
    let mut list = Value::NIL;
    for i in 0..counts.conses {
        list = Value::cons(Value::fixnum((tag * 1_000_000 + i) as i64), list);
    }
    let strings = (0..counts.strings)
        .map(|i| Value::string(format!("s{tag}-{i}")).bits())
        .collect();
    let vectors = (0..counts.vectors)
        .map(|i| {
            let items = (0..counts.vector_len)
                .map(|j| Value::fixnum((tag + i + j) as i64))
                .collect();
            Value::vector(items).bits()
        })
        .collect();
    let tables = (0..counts.tables)
        .map(|t| {
            let table = Value::hash_table(HashTableTest::Equal);
            for i in 0..counts.table_entries {
                let k = key(i);
                let key_value = Value::heap_string(LispString::new(k.clone(), false));
                let hash_key = HashKey::from_str(&k);
                let entry = Value::fixnum((tag * 7 + t + i) as i64);
                table.with_hash_table_mut(|table| {
                    table.insert(hash_key, key_value, entry);
                });
            }
            table.bits()
        })
        .collect();
    let floats = (0..counts.floats)
        .map(|i| Value::make_float(tag as f64 * 1e6 + i as f64 + 0.5).bits())
        .collect();
    Allocated {
        list: list.bits(),
        strings,
        vectors,
        tables,
        floats,
    }
}

/// Check every object `allocate(tag, counts)` made.
fn verify(tag: usize, counts: Counts, got: &Allocated) {
    let mut cell = value(got.list);
    for i in (0..counts.conses).rev() {
        assert_eq!(
            cell.cons_car().as_fixnum(),
            Some((tag * 1_000_000 + i) as i64)
        );
        cell = cell.cons_cdr();
    }
    assert!(cell.is_nil());
    for (i, &bits) in got.strings.iter().enumerate() {
        assert_eq!(
            value(bits).as_utf8_str(),
            Some(format!("s{tag}-{i}").as_str())
        );
    }
    for (i, &bits) in got.vectors.iter().enumerate() {
        let data = value(bits).as_vector_data().expect("a vector");
        assert_eq!(data.len(), counts.vector_len);
        for (j, item) in data.iter().enumerate() {
            assert_eq!(item.as_fixnum(), Some((tag + i + j) as i64));
        }
    }
    for (t, &bits) in got.tables.iter().enumerate() {
        let table = value(bits).as_hash_table().expect("a hash table");
        assert_eq!(table.data.len(), counts.table_entries);
        for i in 0..counts.table_entries {
            let entry = table.data.get(&HashKey::from_str(&key(i))).copied();
            assert_eq!(
                entry.and_then(Value::as_fixnum),
                Some((tag * 7 + t + i) as i64)
            );
        }
    }
    for (i, &bits) in got.floats.iter().enumerate() {
        assert_eq!(
            value(bits).as_float(),
            Some(tag as f64 * 1e6 + i as f64 + 0.5)
        );
    }
}

fn roots(got: &Allocated) -> impl Iterator<Item = TaggedValue> + '_ {
    std::iter::once(got.list)
        .chain(got.strings.iter().copied())
        .chain(got.vectors.iter().copied())
        .chain(got.tables.iter().copied())
        .chain(got.floats.iter().copied())
        .map(value)
}

/// The heap root as an address an owner thread can carry.
struct HeapRoot(usize);

impl HeapRoot {
    fn of(heap: &TestHeap) -> Self {
        Self(heap.root().as_ptr() as usize)
    }

    /// Install the heap on this thread under `baton` (the stand-in for an
    /// attach: installation writes the heap's store protocol).
    fn install(&self, baton: &Mutex<()>) {
        let _attached = baton.lock().unwrap();
        set_tagged_heap_root(NonNull::new(self.0 as *mut TaggedHeap).unwrap());
    }
}

/// Run `body` on one OS thread per owner state, all allocating at once,
/// each with its state installed; close every state at the end.
fn detached_section<R: Send>(
    heap: &TestHeap,
    states: &mut [Box<OwnerAllocState>],
    body: impl Fn(usize) -> R + Sync,
) -> Vec<R> {
    let root = HeapRoot::of(heap);
    let baton = Mutex::new(());
    let start = Barrier::new(states.len());
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = states
            .iter_mut()
            .enumerate()
            .map(|(tag, state)| {
                let (root, baton, start, body) = (&root, &baton, &start, &body);
                scope.spawn(move || {
                    root.install(baton);
                    let result = {
                        let _installed = state.install();
                        start.wait();
                        body(tag)
                    };
                    // Park: close every run before the keeper may collect.
                    close_owner_regions(state);
                    // Leave the World's observation state under the stand-in
                    // Baton, as an ending Lisp thread does: dropped by this
                    // thread's thread-local destructor, its `Rc` would count
                    // down beside the other owners' at their exits.
                    let mut placeholder =
                        crate::tagged::collection_reads::world_state_placeholder();
                    {
                        let _attached = baton.lock().unwrap();
                        crate::tagged::collection_reads::swap_world_state(&mut *placeholder);
                        drop(placeholder);
                    }
                    result
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect()
    });
    // The keeper attaches again: reinstall its heap and adopt.
    heap.install();
    results
}

#[test]
fn two_owner_threads_allocate_every_class_at_once() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    let mut states = vec![heap.new_owner_alloc_state(), heap.new_owner_alloc_state()];
    let got = detached_section(&heap, &mut states, |tag| allocate(tag + 1, FULL));
    heap.close_alloc_regions();
    for (tag, got) in got.iter().enumerate() {
        verify(tag + 1, FULL, got);
    }
    let stats = heap.owner_alloc_stats();
    assert!(stats.refills > 0);
    assert!(stats.adoptions > 0);
    // Adoption registered every owner block, page and object.
    for got in &got {
        let cell = value(got.list).as_cons_ptr().unwrap();
        let base = ConsBlock::block_base_for_ptr(cell);
        assert!(heap.cons_block_index_by_base.contains_key(&base));
        let string = value(got.strings[0]).as_string_ptr().unwrap();
        assert!(heap.string_arena.owns(string.cast()));
        let float = value(got.floats[0]).as_float_ptr().unwrap();
        assert!(heap.float_arena.owns(float.cast()));
        let vector = value(got.vectors[0]).as_veclike_ptr().unwrap();
        assert!(heap.vector_arena.owns(vector.cast()));
        assert!(heap.vector_object_addrs.contains(&(vector as usize)));
        let table = value(got.tables[0]).as_veclike_ptr().unwrap();
        assert!(heap.non_cons_object_addrs.contains(&(table as usize)));
    }
    // Every owner object is the heap's now: a collection keeps them all.
    heap.collect_exact(got.iter().flat_map(roots));
    for (tag, got) in got.iter().enumerate() {
        verify(tag + 1, FULL, got);
    }
    let floats = got.iter().map(|got| got.floats.len()).sum::<usize>();
    assert!(heap.float_arena.pages.len() * FLOAT_PAGE_SLOTS >= floats);
}

#[test]
fn refills_under_contention_keep_every_object() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    let small = OwnerRunSizes {
        cons_cells: 4,
        slots: 2,
    };
    let mut states: Vec<_> = (0..8)
        .map(|_| heap.new_owner_alloc_state_with(small))
        .collect();
    let counts = Counts {
        conses: 2_000,
        strings: 500,
        vectors: 200,
        vector_len: 3,
        tables: 4,
        table_entries: 50,
        floats: 500,
    };
    let got = detached_section(&heap, &mut states, |tag| allocate(tag, counts));
    heap.close_alloc_regions();
    for (tag, got) in got.iter().enumerate() {
        verify(tag, counts, got);
    }
    let stats = heap.owner_alloc_stats();
    // 8 owners x (500 conses + 250 + 250 + 100 slot runs) at least.
    assert!(stats.refills >= 8 * 1_100, "refills: {stats:?}");
    heap.collect_exact(got.iter().flat_map(roots));
    for (tag, got) in got.iter().enumerate() {
        verify(tag, counts, got);
    }
}

#[test]
fn close_and_reopen_around_a_collection() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    let counts = Counts {
        conses: 20_000,
        strings: 2_000,
        vectors: 200,
        vector_len: 10,
        tables: 10,
        table_entries: 100,
        floats: 5_000,
    };
    let mut states = vec![heap.new_owner_alloc_state(), heap.new_owner_alloc_state()];
    // Each owner keeps one set and drops another, then parks closed.
    let first = detached_section(&heap, &mut states, |tag| {
        (allocate(tag, counts), allocate(tag + 10, counts))
    });
    // Every owner is parked with its runs closed: collect as the keeper.
    heap.collect_exact(
        first
            .iter()
            .flat_map(|(kept, dropped)| roots(kept).chain(roots(dropped))),
    );
    let with_garbage = heap.live_bytes();
    heap.collect_exact(first.iter().flat_map(|(kept, _)| roots(kept)));
    let without = heap.live_bytes();
    assert!(
        without < with_garbage,
        "live bytes {without} after dropping the owners' garbage, {with_garbage} before"
    );
    for (tag, (kept, _)) in first.iter().enumerate() {
        verify(tag, counts, kept);
    }
    // Resume: runs reopen lazily with the new parity; allocate more.
    for state in &mut states {
        reopen_owner_regions(state);
    }
    let second = detached_section(&heap, &mut states, |tag| allocate(tag + 20, counts));
    heap.collect_exact(
        first
            .iter()
            .flat_map(|(kept, _)| roots(kept))
            .chain(second.iter().flat_map(roots)),
    );
    for (tag, (kept, _)) in first.iter().enumerate() {
        verify(tag, counts, kept);
    }
    for (tag, got) in second.iter().enumerate() {
        verify(tag + 20, counts, got);
    }
}

#[test]
fn keeper_without_owners_takes_the_old_path() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    let got = allocate(
        3,
        Counts {
            conses: 1_000,
            strings: 100,
            vectors: 10,
            vector_len: 5,
            tables: 2,
            table_entries: 10,
            floats: 300,
        },
    );
    assert!(heap.owner_pages.is_none());
    assert_eq!(heap.owner_alloc_stats(), Default::default());
    assert!(heap.region_stats.cons_refills > 0);
    assert!(heap.region_stats.float_refills > 0);
    heap.collect_exact(roots(&got));
    assert!(heap.owner_pages.is_none());
}

/// The `tag`-th owner's `i`-th bignum: past the fixnum range.
fn owner_bignum(tag: usize, i: usize) -> Integer {
    Integer::from(u64::MAX) * Integer::from(tag as u64 + 1) + Integer::from(i as u64)
}

#[test]
fn owner_threads_allocate_bignums_that_pace_and_are_adopted() {
    crate::test_utils::init_test_tracing();
    const N: usize = 5_000;
    let mut heap = TestHeap::new();
    heap.install();
    let mut states = vec![heap.new_owner_alloc_state(), heap.new_owner_alloc_state()];
    let before = heap.bytes_since_gc();
    let got = detached_section(&heap, &mut states, |tag| {
        (0..N)
            .map(|i| Value::make_integer(owner_bignum(tag, i)).bits())
            .collect::<Vec<_>>()
    });
    // Parked with their runs closed, before any adoption: the keeper's
    // pacing already counts the owners' bignums.
    let pending = 2 * N * size_of::<BignumObj>();
    assert!(heap.bytes_since_gc() - before >= pending);
    heap.close_alloc_regions();
    assert_eq!(
        heap.owner_pages.as_deref().map(OwnerPages::pending_bytes),
        Some(0)
    );
    assert!(
        heap.bytes_since_gc() - before >= pending,
        "adoption charged them"
    );
    for (tag, bits) in got.iter().enumerate() {
        for (i, &bits) in bits.iter().enumerate() {
            let ptr = value(bits).as_veclike_ptr().expect("a bignum");
            assert!(heap.bignum_arena.owns(ptr.cast()));
            assert_eq!(value(bits).as_bignum(), Some(&owner_bignum(tag, i)));
        }
    }
    // The heap's own now: a collection keeps the rooted bignums, and
    // reclaims the second owner's once only the first's are rooted.
    heap.collect_exact(got.iter().flatten().copied().map(value));
    let with_garbage = heap.live_bytes();
    heap.collect_exact(got[0].iter().copied().map(value));
    let without = heap.live_bytes();
    assert!(
        without < with_garbage,
        "live bytes {without} after dropping the second owner's bignums, {with_garbage} before"
    );
    for (i, &bits) in got[0].iter().enumerate() {
        assert_eq!(value(bits).as_bignum(), Some(&owner_bignum(0, i)));
    }
}

/// One owner allocates N conses into a list it keeps and N floats it keeps;
/// returns the list's and the floats' bits and the refills and cold-mutex
/// acquisitions the section added (the owner's close included).
fn one_owner_conses_and_floats(heap: &mut TestHeap, n: usize) -> (usize, Vec<usize>, u64, u64) {
    let mut states = vec![heap.new_owner_alloc_state()];
    let before = heap.owner_alloc_stats();
    let mut got = detached_section(heap, &mut states, |_| {
        let mut list = Value::NIL;
        for i in 0..n {
            list = Value::cons(Value::fixnum(i as i64), list);
        }
        let floats = (0..n)
            .map(|i| Value::make_float(i as f64 + 0.5).bits())
            .collect::<Vec<_>>();
        (list.bits(), floats)
    });
    heap.close_alloc_regions();
    let after = heap.owner_alloc_stats();
    let (list, floats) = got.pop().expect("one owner");
    (
        list,
        floats,
        after.refills - before.refills,
        after.locks - before.locks,
    )
}

/// A refill hands an owner a whole cons block or object page of its own:
/// N allocations of one class cost about N over the block's cells (or the
/// page's slots) refills, and none of them takes the owners' one mutex,
/// which the owner takes once, to hand everything over when it closes.
#[test]
fn an_owner_refills_once_per_cons_block_and_float_page() {
    crate::test_utils::init_test_tracing();
    const BLOCKS: usize = 8;
    let mut heap = TestHeap::new();
    heap.install();
    let float_page = ObjectPage::<FloatObj>::SLOTS;
    let n = BLOCKS * CONS_BLOCK_SIZE.max(float_page);
    let (list, floats, refills, locks) = one_owner_conses_and_floats(&mut heap, n);
    let mut cell = value(list);
    for i in (0..n).rev() {
        assert_eq!(cell.cons_car().as_fixnum(), Some(i as i64));
        cell = cell.cons_cdr();
    }
    assert!(cell.is_nil());
    for (i, &bits) in floats.iter().enumerate() {
        assert_eq!(value(bits).as_float(), Some(i as f64 + 0.5));
    }
    // One refill per block and page the allocations fill or start.
    let bound = n.div_ceil(CONS_BLOCK_SIZE) + n.div_ceil(float_page);
    assert!(
        refills <= bound as u64,
        "{n} conses and {n} floats took {refills} refills, more than {bound}"
    );
    assert_eq!(locks, 1, "{refills} refills took {locks} cold-mutex locks");
}

/// The owner allocator alone, in a test build: one owner allocating
/// everything, then 8 owners at once allocating the same total, each a
/// short list of conses (garbage) and floats. Prints wall time, refills and
/// the cold mutex's contended wait; asserts only the allocations.
#[test]
#[ignore = "a measurement: run with --run-ignored only"]
fn measure_owner_allocation_one_owner_versus_eight() {
    crate::test_utils::init_test_tracing();
    const TOTAL: usize = 8_000_000;
    let mut heap = TestHeap::new();
    heap.install();
    for owners in [1_usize, 8] {
        let mut states: Vec<_> = (0..owners).map(|_| heap.new_owner_alloc_state()).collect();
        let before = heap.owner_alloc_stats();
        let started = std::time::Instant::now();
        let sums = detached_section(&heap, &mut states, |_| {
            let mut list = Value::NIL;
            let mut sum = 0.0;
            for i in 0..TOTAL / owners {
                list = if i % 64 == 0 {
                    Value::NIL
                } else {
                    Value::cons(Value::fixnum(i as i64), list)
                };
                sum += Value::make_float(i as f64).as_float().unwrap_or(0.0);
            }
            sum
        });
        let elapsed = started.elapsed();
        heap.close_alloc_regions();
        let after = heap.owner_alloc_stats();
        assert_eq!(sums.len(), owners);
        println!(
            "| {owners} owners | {elapsed:.2?} | refills {} | locks {} | lock wait {} ns |",
            after.refills - before.refills,
            after.locks - before.locks,
            after.lock_wait_ns - before.lock_wait_ns
        );
    }
}

/// One owner allocates N conses and N floats, all garbage (the list is
/// dropped every 64 cells), and returns the base of every cons block and
/// float page it took them from.
fn garbage_bases(heap: &TestHeap, states: &mut [Box<OwnerAllocState>], n: usize) -> Vec<usize> {
    let mut bases = detached_section(heap, states, |_| {
        let mut bases = std::collections::BTreeSet::new();
        let mut list = Value::NIL;
        for i in 0..n {
            list = if i % 64 == 0 {
                Value::NIL
            } else {
                Value::cons(Value::fixnum(i as i64), list)
            };
            if let Some(cell) = list.as_cons_ptr() {
                bases.insert(ConsBlock::block_base_for_ptr(cell));
            }
            let float = Value::make_float(i as f64).as_float_ptr().expect("a float");
            bases.insert(float as usize & !(OBJECT_PAGE_ALIGN - 1));
        }
        bases
    });
    bases.pop().expect("one owner").into_iter().collect()
}

/// After a collection that empties the cons blocks and float pages an owner
/// carved, the owner's next refills carve that swept storage again instead
/// of asking the allocator for fresh blocks and pages.
#[test]
fn an_owners_refills_after_a_collection_reuse_the_swept_blocks_and_pages() {
    crate::test_utils::init_test_tracing();
    const BLOCKS: usize = 4;
    let mut heap = TestHeap::new();
    heap.install();
    let n = BLOCKS * CONS_BLOCK_SIZE.max(ObjectPage::<FloatObj>::SLOTS);
    let mut states = vec![heap.new_owner_alloc_state()];
    let first = garbage_bases(&heap, &mut states, n);
    // Everything the owner allocated is garbage: the sweep empties its
    // blocks and pages.
    heap.collect_exact(std::iter::empty());
    reopen_owner_regions(&mut states[0]);
    let fresh_pages = <FloatObj as PagedObject>::live_page_counter().load(Ordering::Relaxed);
    let second = garbage_bases(&heap, &mut states, n);
    let fresh_pages =
        <FloatObj as PagedObject>::live_page_counter().load(Ordering::Relaxed) - fresh_pages;
    assert_eq!(
        fresh_pages, 0,
        "the second round took {fresh_pages} float pages from the allocator"
    );
    let unseen: Vec<_> = second.iter().filter(|base| !first.contains(base)).collect();
    assert!(
        unseen.is_empty(),
        "the second round carved {} of {} blocks and pages the first had not: {unseen:x?}",
        unseen.len(),
        second.len()
    );
}

/// An owner's close stamps every page it carved with the owner, and the
/// arena's owner runs cut its pages where the owner changes: two owners'
/// strings, adopted, come back as runs of one owner's pages each.
#[test]
fn a_close_stamps_an_owners_pages_and_owner_runs_cut_at_each_owner() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    let mut states = vec![heap.new_owner_alloc_state(), heap.new_owner_alloc_state()];
    let keys = detached_section(&heap, &mut states, |tag| {
        for i in 0..3 * ObjectPage::<StringObj>::SLOTS {
            Value::string(format!("s{tag}-{i}"));
        }
        current_owner_key()
    });
    assert_ne!(keys[0], keys[1]);
    assert!(!keys.contains(&OwnerKey::KEEPER));
    heap.close_alloc_regions();
    // The pages' owners, grouped where they change.
    let mut expected: Vec<(OwnerKey, usize)> = Vec::new();
    for page in &heap.string_arena.pages {
        match expected.last_mut() {
            Some((owner, len)) if *owner == page.owner => *len += 1,
            _ => expected.push((page.owner, 1)),
        }
    }
    assert!(
        keys.iter()
            .all(|key| expected.iter().any(|(owner, _)| owner == key))
    );
    let pages = heap.string_arena.pages.len();
    let runs = heap.string_arena.owner_runs(0, pages);
    let got: Vec<(OwnerKey, usize)> = runs
        .iter()
        .map(|(owner, run)| (*owner, run.len()))
        .collect();
    assert_eq!(got, expected);
    let (owner, run) = runs
        .into_iter()
        .find(|(_, run)| run.len() > 1)
        .expect("a run of pages");
    let len = run.len();
    let (head, tail) = run.split(1);
    assert_eq!(
        (head.len(), tail.map(|tail| tail.len())),
        (1, Some(len - 1)),
        "{owner:?}"
    );
}

/// A collection's in-stop sweep gives each owner's thread the pages that
/// owner carved: four owners leave 3000, 6000, 9000 and 12000 dead strings
/// and as many dead bignums, and each thread, taking a share with its own
/// state installed, holds for after the stop the payloads of exactly what
/// its owner allocated.
#[test]
fn the_in_stop_sweep_gives_each_owner_the_pages_it_carved() {
    crate::test_utils::init_test_tracing();
    let mut heap = TestHeap::new();
    heap.install();
    // SAFETY: serial fixture heap; no collector or compiled leaf runs.
    unsafe { heap.set_generational_enabled(true) };
    heap.partition_dump = false;
    heap.dump_blackened = false;
    heap.set_gc_threshold(usize::MAX);
    let garbage = |tag: usize| (tag + 1) * 3_000;
    let mut states: Vec<_> = (0..4).map(|_| heap.new_owner_alloc_state()).collect();
    detached_section(&heap, &mut states, |tag| {
        for i in 0..garbage(tag) {
            Value::string(format!("garbage {tag}-{i}"));
            // At least 2^64: a limb vector to free.
            Value::make_integer(Integer::from(tag * 100_000 + i + 1) << 64_u64);
        }
    });
    heap.close_alloc_regions();
    heap.begin_minor_collection().unwrap();
    heap.complete_minor_collection();
    assert!(heap.sweep_in_progress());
    heap.sweep_in_stop_on(std::num::NonZeroUsize::new(4).unwrap(), |parts, share| {
        assert!(parts >= 4, "a share per owner, {parts} in all");
        std::thread::scope(|threads| {
            let mut states = states.iter_mut();
            for _ in 0..parts {
                let state = states.next();
                threads.spawn(move || {
                    let _installed = state.map(|state| state.install());
                    share(0);
                });
            }
        });
    });
    for (tag, state) in states.iter().enumerate() {
        assert_eq!(
            state.swept_payload_count(),
            2 * garbage(tag),
            "owner {tag}'s thread holds its own garbage's payloads"
        );
    }
    heap.finish_incremental_sweep_now();
    for state in &mut states {
        state.free_swept_payloads();
    }
}

/// Adopting an owner page threads exactly its never-handed slots below the
/// bump cursor onto the page's free list, descending, across bitmap words:
/// here slots 0..150 were reserved and 3..70 and 100..129 handed out.
#[test]
fn adoption_threads_exactly_the_never_handed_slots() {
    let mut page = ObjectPage::<FloatObj>::new();
    page.next_index = 150;
    page.set_allocated_range(3, 67);
    page.set_allocated_range(100, 29);
    let mut arena = ObjectArena::<FloatObj>::new(None);
    let index = arena.adopt_page(page);
    let page = &arena.pages[index];
    let mut threaded = Vec::new();
    let mut slot = page.free_head;
    while slot != PAGE_NONE {
        threaded.push(slot);
        // SAFETY: a free slot's link word, written by the adoption.
        slot = unsafe { page.free_link_ptr(slot).read() };
    }
    let expected: Vec<usize> = (0..150)
        .rev()
        .filter(|&slot| !(3..70).contains(&slot) && !(100..129).contains(&slot))
        .collect();
    assert_eq!(threaded, expected);
    assert!(page.on_partial);
}
