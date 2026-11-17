//! Per-owner allocation (shared-heap experiment, stage 4).
//!
//! The heap keeps ONE collector and ONE set of block and page registries. The
//! keeper (the thread holding the World's Baton) allocates exactly as before:
//! its cursors stay in `JitHeapState`. Every OTHER owner thread allocates from
//! runs it alone writes: a run of cons cells in a cons block, of float,
//! string or vector slots in a 64 KiB page. Those blocks and pages are NOT in
//! the heap's registries while owners carve them; they sit in [`OwnerPages`]'s
//! cold state behind one `parking_lot::Mutex`, which an owner takes only to
//! close one run and carve the next (a refill). Boxed vector-likes (hash
//! tables) are `Box`es whose `all_objects` link waits in the same cold state.
//!
//! The keeper ADOPTS what the owners left there: under the same mutex it moves
//! every block and page with no open run into the heap's registries
//! ([`TaggedHeap::adopt_owner_pages`], called from `close_alloc_regions`, so
//! every collector entry adopts before it marks). Owner allocation therefore
//! never touches a structure the keeper mutates without that mutex.
//!
//! Collection contract (slice S4): every owner calls
//! [`close_owner_regions`] when it parks for a collection and allocates again
//! only after the collection finished ([`reopen_owner_regions`] is lazy). With
//! every run closed, the collection's first `close_alloc_regions` adopts every
//! owner block and page, so mark and sweep see one heap. The mark parity an
//! owner stamps is read at each refill; it changes only inside a collection.
//!
//! Everything here is `#[cfg(feature = "shared-heap")]`; without an installed
//! [`OwnerAllocState`] the allocation entry points take the keeper path.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// How many cells or slots an owner takes per refill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnerRunSizes {
    pub(crate) cons_cells: usize,
    pub(crate) slots: usize,
}

impl OwnerRunSizes {
    /// 16 KiB of conses, a quarter page of slots per class.
    pub(crate) const DEFAULT: Self = Self {
        cons_cells: 1024,
        slots: 256,
    };
}

/// The owner allocator's lock counters (plain integers).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwnerAllocStats {
    /// Cold-mutex acquisitions by owners (refills and closes).
    pub(crate) refills: u64,
    /// Nanoseconds owners waited for the cold mutex.
    pub(crate) lock_wait_ns: u64,
    /// Keeper adoptions that moved at least one block, page or object.
    pub(crate) adoptions: u64,
}

/// The size classes an owner carves runs of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerClass {
    Cons,
    Float,
    String,
    Vector,
}

/// One open run: `[start, lim)` reserved, `[start, cur)` handed out.
/// All zero when closed.
#[derive(Clone, Copy, Debug, Default)]
struct OwnerRun {
    start: usize,
    cur: usize,
    lim: usize,
}

impl OwnerRun {
    #[inline(always)]
    fn take(&mut self, stride: usize) -> Option<usize> {
        let cur = self.cur;
        if cur < self.lim {
            self.cur = cur + stride;
            Some(cur)
        } else {
            None
        }
    }
}

/// Consing-counter charges not yet folded into the heap.
#[derive(Clone, Copy, Debug, Default)]
struct Charges {
    counts: [u64; MEMORY_USE_COUNT_LEN],
    allocated: usize,
    bytes: usize,
    cons_live: usize,
}

impl Charges {
    fn add(&mut self, other: &Charges) {
        for (sum, n) in self.counts.iter_mut().zip(other.counts) {
            *sum = sum.wrapping_add(n);
        }
        self.allocated += other.allocated;
        self.bytes += other.bytes;
        self.cons_live += other.cons_live;
    }

    fn count(&mut self, slot: MemoryUseCountSlot, n: u64) {
        let count = &mut self.counts[slot.index()];
        *count = count.wrapping_add(n);
    }
}

/// A cons block owners carve, with what its closed runs handed out.
struct PendingBlock {
    block: ConsBlock,
    open_runs: usize,
    /// `(first cell index, cells)` of every closed run's handed-out head.
    handed: Vec<(usize, usize)>,
    /// `(first cell index, cells)` of every closed run's unused tail.
    unused: Vec<(usize, usize)>,
}

/// A page of class `T` owners carve. Alloc bits are set, under the mutex, for
/// exactly the slots closed runs handed out.
struct PendingPage<T: PagedObject> {
    page: ObjectPage<T>,
    open_runs: usize,
}

/// One class's pending pages and the base of the page being carved.
struct PagePool<T: PagedObject> {
    pages: Vec<PendingPage<T>>,
    carving: Option<usize>,
}

impl<T: PagedObject> PagePool<T> {
    const fn new() -> Self {
        Self {
            pages: Vec::new(),
            carving: None,
        }
    }

    fn page_mut(&mut self, base: usize) -> &mut PendingPage<T> {
        self.pages
            .iter_mut()
            .rev()
            .find(|pending| pending.page.base_addr() == base)
            .expect("an owner run names a pending page")
    }

    /// Reserve up to `want` never-used slots: the carving page's tail, else a
    /// fresh page.
    fn carve(&mut self, want: usize) -> OwnerRun {
        let open = self
            .carving
            .filter(|&base| self.page_mut(base).page.next_index < ObjectPage::<T>::SLOTS);
        let base = match open {
            Some(base) => base,
            None => {
                let page = ObjectPage::new();
                let base = page.base_addr();
                self.pages.push(PendingPage { page, open_runs: 0 });
                self.carving = Some(base);
                base
            }
        };
        let pending = self.page_mut(base);
        let first = pending.page.next_index;
        let count = want.min(ObjectPage::<T>::SLOTS - first);
        pending.page.next_index += count;
        pending.open_runs += 1;
        let start = pending.page.slot_ptr(first) as usize;
        OwnerRun {
            start,
            cur: start,
            lim: start + count * T::SLOT_BYTES,
        }
    }

    /// Close `run`: set the alloc bits of its handed-out slots. Returns how
    /// many it handed out.
    fn close(&mut self, run: OwnerRun) -> usize {
        let base = run.start & !(OBJECT_PAGE_ALIGN - 1);
        let pending = self.page_mut(base);
        let used = (run.cur - run.start) / T::SLOT_BYTES;
        if used > 0 {
            pending
                .page
                .set_allocated_range((run.start - base) / T::SLOT_BYTES, used);
        }
        pending.open_runs -= 1;
        used
    }

    /// Take every page no run is open in; the carving page too when `all`
    /// (no run is open anywhere), after which owners start a fresh page.
    fn drain(&mut self, all: bool) -> Vec<ObjectPage<T>> {
        let carving = self.carving;
        let (adopted, kept) = std::mem::take(&mut self.pages)
            .into_iter()
            .partition::<Vec<_>, _>(|pending| {
                pending.open_runs == 0 && (all || Some(pending.page.base_addr()) != carving)
            });
        if all {
            self.carving = None;
        }
        self.pages = kept;
        adopted.into_iter().map(|pending| pending.page).collect()
    }
}
