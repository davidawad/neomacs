//! The stop-the-world sweep of an explicit collection, spread over threads.
//!
//! `garbage-collect` marks on the GC thread and then sweeps on the caller:
//! every object page and cons block in turn, one thread. A page's slots and a
//! block's cells are independent of every other's, and the world is stopped,
//! so the sweeps below cut the work into disjoint runs and sweep them on
//! scoped threads. The serial code keeps what is not independent: linking
//! swept pages onto the partial chains (in page order, as the serial sweep
//! does), publishing each block's dead cells, releasing empty blocks, the
//! young object list, and the classes whose dead payloads touch more than
//! allocator memory (markers, bytecode, macros, symbols with position).
//!
//! `NEOVM_GC_SWEEP_THREADS` picks the thread count (`0` or `1` is serial).

use super::arena_pages::{ArenaSweepRanges, ObjectArena, PageRange, PageRun, SweptRun};
use super::incremental::{PreSwept, SweptPayload};
use super::*;

/// Fewer pages of one class than this per thread are swept sooner than a
/// thread starts for them.
#[cfg(not(test))]
const PAGES_PER_THREAD: usize = 64;
/// Tests sweep small heaps on threads.
#[cfg(test)]
const PAGES_PER_THREAD: usize = 2;
/// As [`PAGES_PER_THREAD`], for cons blocks.
#[cfg(not(test))]
const CONS_BLOCKS_PER_THREAD: usize = 64;
#[cfg(test)]
const CONS_BLOCKS_PER_THREAD: usize = 2;

/// Shares run on a thread of their own, in this process (tests assert that
/// the threaded path ran).
#[cfg(test)]
pub(super) static SPAWNED_SHARES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Cons blocks one pre-sweep thread sweeps, each with its position in the
/// order the serial pass visits them.
struct BlockShare<'a>(Vec<(usize, &'a mut ConsBlock)>);

// SAFETY: the shares of one pre-sweep are disjoint, the world is stopped, and
// a block's sweep reads and writes only its own storage (no collection
// observation is active: the callers fall back to the serial sweep then).
unsafe impl Send for BlockShare<'_> {}

/// Run each of SHARES on a thread (the first on this one) and collect what
/// they return, in order.
fn run_in_threads<S: Send, R: Send>(shares: Vec<S>, work: impl Fn(S) -> R + Sync) -> Vec<R> {
    let mut shares = shares.into_iter();
    let Some(first) = shares.next() else {
        return Vec::new();
    };
    std::thread::scope(|threads| {
        let work = &work;
        let handles: Vec<_> = shares
            .map(|share| {
                #[cfg(test)]
                SPAWNED_SHARES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                threads.spawn(move || work(share))
            })
            .collect();
        let mut results = vec![work(first)];
        results.extend(handles.into_iter().map(|handle| {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
        }));
        results
    })
}

/// Sweep every page of ARENA on up to WORKERS threads, as
/// `ObjectArena::sweep_range` over the whole arena would, collecting the
/// freed slots' addresses into FREED_SLOTS when asked. `None` when the arena
/// is too small to be worth a thread (nothing was swept).
fn sweep_class_in_threads<T: SweptPayload, const GENERATIONAL: bool>(
    arena: &mut ObjectArena<T>,
    workers: usize,
    parity: MarkParity,
    scope: CollectionScope,
    freed_slots: Option<&mut Vec<usize>>,
) -> Option<(usize, usize)> {
    let pages = arena.pages.len();
    let workers = workers.min(pages / PAGES_PER_THREAD);
    if workers < 2 {
        return None;
    }
    let collect = freed_slots.is_some();
    let chunk = pages.div_ceil(workers);
    let mut shares: Vec<Vec<PageRun<'_, T>>> = (0..workers).map(|_| Vec::new()).collect();
    let mut next = 0usize;
    for (_, mut run) in arena.owner_runs(0, pages) {
        loop {
            let (head, tail) = run.split(chunk);
            shares[next % workers].push(head);
            next += 1;
            match tail {
                Some(tail) => run = tail,
                None => break,
            }
        }
    }
    let mut swept: Vec<SweptRun> = run_in_threads(shares, |runs| {
        runs.into_iter()
            .map(|run| run.sweep::<GENERATIONAL>(parity, scope, collect, None))
            .collect::<Vec<_>>()
    })
    .into_iter()
    .flatten()
    .collect();
    // Linked in page order, as the serial sweep links them.
    swept.sort_unstable_by_key(|run| run.first);
    Some(arena.link_swept_runs(swept, freed_slots))
}

impl TaggedHeap {
    /// How many threads an eager sweep may use now: one when the heap is
    /// observed (the observations need the serial walk), else the knob.
    fn eager_sweep_workers(&self) -> usize {
        if has_collection_observations() || has_noncons_collection_observations() {
            return 1;
        }
        knobs::sweep_threads()
    }

    /// The eager sweep of every object arena: the classes whose dead payloads
    /// only free allocator memory on threads, the rest serially.
    /// Returns `(survivor bytes, slots freed)`, as `sweep_arena_pages_ranges`.
    pub(super) fn sweep_arena_pages_eager(&mut self) -> (usize, usize) {
        let workers = self.eager_sweep_workers();
        let mut ranges = ArenaSweepRanges {
            float: PageRange::all(self.float_arena.pages.len()),
            string: PageRange::all(self.string_arena.pages.len()),
            vector: PageRange::all(self.vector_arena.pages.len()),
            bytecode: PageRange::all(self.bytecode_arena.pages.len()),
            lambda: PageRange::all(self.lambda_arena.pages.len()),
            macro_: PageRange::all(self.macro_arena.pages.len()),
            record: PageRange::all(self.record_arena.pages.len()),
            symbol_with_pos: PageRange::all(self.symbol_with_pos_arena.pages.len()),
            marker: PageRange::all(self.marker_arena.pages.len()),
            bignum: PageRange::all(self.bignum_arena.pages.len()),
        };
        let mut live = 0usize;
        let mut freed = 0usize;
        if workers > 1 {
            // Collector code never sees an open allocation region
            // (`alloc_region.rs`, invariant I2).
            self.close_alloc_regions();
            let (class_live, class_freed) = if self.generational.enabled {
                self.sweep_classes_in_threads::<true>(workers, &mut ranges)
            } else {
                self.sweep_classes_in_threads::<false>(workers, &mut ranges)
            };
            live += class_live;
            freed += class_freed;
        }
        let (rest_live, rest_freed) = self.sweep_arena_pages_ranges(ranges);
        (live.saturating_add(rest_live), freed + rest_freed)
    }

    /// Sweep the classes that are big enough on threads, and empty the range
    /// of each one swept.
    fn sweep_classes_in_threads<const GENERATIONAL: bool>(
        &mut self,
        workers: usize,
        ranges: &mut ArenaSweepRanges,
    ) -> (usize, usize) {
        let parity = self.mark_parity;
        let scope = if GENERATIONAL {
            self.collection_scope()
        } else {
            CollectionScope::Young
        };
        let mut live = 0usize;
        let mut freed = 0usize;
        macro_rules! class {
            ($arena:ident, $range:ident, $freed_slots:expr) => {
                if let Some((class_live, class_freed)) = sweep_class_in_threads::<_, GENERATIONAL>(
                    &mut self.$arena,
                    workers,
                    parity,
                    scope,
                    $freed_slots,
                ) {
                    live = live.saturating_add(class_live);
                    freed += class_freed;
                    ranges.$range = PageRange::default();
                }
            };
        }
        class!(float_arena, float, None);
        class!(string_arena, string, None);
        class!(lambda_arena, lambda, None);
        class!(record_arena, record, None);
        class!(bignum_arena, bignum, None);
        let mut vectors = Vec::new();
        class!(vector_arena, vector, Some(&mut vectors));
        for addr in vectors {
            let removed = self.vector_object_addrs.remove(&addr);
            debug_assert!(removed, "freed page vector was not in the registry");
        }
        (live, freed)
    }

    /// Sweep, on threads, the cons blocks at ORDER (indices into
    /// `cons_blocks`, in the order the serial pass visits them), rebuilding
    /// each block's dead-cell list without publishing it. The result is
    /// indexed by position in ORDER; `None` when the blocks are too few for
    /// threads or a collection observation needs the serial walk.
    pub(super) fn presweep_cons_blocks(
        &mut self,
        order: &[usize],
        generational: bool,
    ) -> Option<Vec<PreSwept>> {
        let workers = self
            .eager_sweep_workers()
            .min(order.len() / CONS_BLOCKS_PER_THREAD);
        if workers < 2 {
            return None;
        }
        let mut wanted: Vec<(usize, usize)> = order
            .iter()
            .enumerate()
            .map(|(position, &index)| (index, position))
            .collect();
        wanted.sort_unstable();
        let mut wanted = wanted.into_iter().peekable();
        let chunk = order.len().div_ceil(workers);
        let mut shares: Vec<BlockShare<'_>> =
            (0..workers).map(|_| BlockShare(Vec::new())).collect();
        for (index, block) in self.cons_blocks.iter_mut().enumerate() {
            if let Some((_, position)) = wanted.next_if(|&(wanted, _)| wanted == index) {
                shares[position / chunk].0.push((position, block));
            }
        }
        assert!(
            wanted.next().is_none(),
            "each pre-swept cons block is found once"
        );
        let swept = run_in_threads(shares, |BlockShare(blocks)| {
            blocks
                .into_iter()
                .map(|(position, block)| {
                    let mut head = std::ptr::null_mut();
                    let live = if generational {
                        block.sweep_generational(&mut head)
                    } else {
                        block.sweep(&mut head)
                    };
                    (position, PreSwept { head, live })
                })
                .collect::<Vec<_>>()
        });
        let mut by_position = vec![
            PreSwept {
                head: std::ptr::null_mut(),
                live: 0,
            };
            order.len()
        ];
        for (position, swept) in swept.into_iter().flatten() {
            by_position[position] = swept;
        }
        Some(by_position)
    }
}
