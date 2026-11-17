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

/// The cold allocator state, only ever touched under [`OwnerPages::cold`].
struct AllocatorCold {
    blocks: Vec<PendingBlock>,
    /// Base of the cons block being carved.
    carving_block: Option<usize>,
    floats: PagePool<FloatObj>,
    strings: PagePool<StringObj>,
    vectors: PagePool<VectorObj>,
    /// Owner-allocated boxed vector-likes awaiting their `all_objects` link.
    boxed: Vec<*mut VecLikeHeader>,
    charges: Charges,
    open_runs: usize,
}

// SAFETY: the raw pointers name block and page storage and boxed objects this
// state owns exclusively until adoption hands them to the keeper's heap; they
// are read and written only under `OwnerPages::cold`, from whichever thread
// holds it. No thread-affine data (no Rc, no TLS-derived pointer) is held.
unsafe impl Send for AllocatorCold {}

/// A cons block adoption registers, with its closed runs.
pub(super) struct AdoptedBlock {
    pub(super) block: ConsBlock,
    /// `(first cell index, cells)` handed out.
    pub(super) handed: Vec<(usize, usize)>,
    /// `(first cell index, cells)` never handed out.
    pub(super) unused: Vec<(usize, usize)>,
}

/// What one adoption moves into the heap.
struct Adoption {
    blocks: Vec<AdoptedBlock>,
    floats: Vec<ObjectPage<FloatObj>>,
    strings: Vec<ObjectPage<StringObj>>,
    vectors: Vec<ObjectPage<VectorObj>>,
    boxed: Vec<*mut VecLikeHeader>,
    charges: Charges,
}

impl AllocatorCold {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            carving_block: None,
            floats: PagePool::new(),
            strings: PagePool::new(),
            vectors: PagePool::new(),
            boxed: Vec::new(),
            charges: Charges::default(),
            open_runs: 0,
        }
    }

    fn carve(&mut self, class: OwnerClass, sizes: OwnerRunSizes) -> OwnerRun {
        self.open_runs += 1;
        match class {
            OwnerClass::Cons => self.carve_cons(sizes.cons_cells),
            OwnerClass::Float => self.floats.carve(sizes.slots),
            OwnerClass::String => self.strings.carve(sizes.slots),
            OwnerClass::Vector => self.vectors.carve(sizes.slots),
        }
    }

    fn block_mut(&mut self, base: usize) -> &mut PendingBlock {
        self.blocks
            .iter_mut()
            .rev()
            .find(|pending| pending.block.base_addr() == base)
            .expect("an owner run names a pending cons block")
    }

    fn carve_cons(&mut self, want: usize) -> OwnerRun {
        let reserved = self
            .carving_block
            .map(|base| (base, self.block_mut(base).block.reserve_tail(want)))
            .filter(|&(_, (_, n))| n > 0);
        let (base, first, n) = match reserved {
            Some((base, (first, n))) => (base, first, n),
            None => {
                let mut block = ConsBlock::new();
                let (first, n) = block.reserve_tail(want);
                let base = block.base_addr();
                self.blocks.push(PendingBlock {
                    block,
                    open_runs: 0,
                    handed: Vec::new(),
                    unused: Vec::new(),
                });
                self.carving_block = Some(base);
                (base, first, n)
            }
        };
        let pending = self.block_mut(base);
        pending.open_runs += 1;
        // SAFETY: `first + n` is within the block's cells (`reserve_tail`).
        let start = unsafe { pending.block.cells_ptr().add(first) } as usize;
        OwnerRun {
            start,
            cur: start,
            lim: start + n * size_of::<ConsCell>(),
        }
    }

    /// Close one owner run and charge what it handed out.
    fn close(&mut self, class: OwnerClass, run: OwnerRun) {
        if run.start == 0 {
            return;
        }
        self.open_runs -= 1;
        let mut charges = Charges::default();
        match class {
            OwnerClass::Cons => {
                let pending = self.block_mut(run.start & !(CONS_BLOCK_ALIGN - 1));
                let first = ConsBlock::index_of_ptr(run.start as *const ConsCell);
                let used = (run.cur - run.start) / size_of::<ConsCell>();
                let unused = (run.lim - run.cur) / size_of::<ConsCell>();
                if used > 0 {
                    pending.handed.push((first, used));
                }
                if unused > 0 {
                    pending.unused.push((first + used, unused));
                }
                pending.open_runs -= 1;
                charges.count(MemoryUseCountSlot::ConsCells, used as u64);
                charges.allocated += used;
                charges.cons_live += used;
                charges.bytes += used * size_of::<ConsCell>();
            }
            OwnerClass::Float => {
                let used = self.floats.close(run);
                charges.count(MemoryUseCountSlot::Floats, used as u64);
                charges.allocated += used;
                charges.bytes += used * size_of::<FloatObj>();
            }
            OwnerClass::String => {
                self.strings.close(run);
            }
            OwnerClass::Vector => {
                self.vectors.close(run);
            }
        }
        self.charges.add(&charges);
    }

    /// Everything no open run still writes; with no run open anywhere, the
    /// carving blocks and pages as well (owners start fresh ones).
    fn drain(&mut self) -> Adoption {
        let all = self.open_runs == 0;
        let carving = self.carving_block;
        let (blocks, kept) = std::mem::take(&mut self.blocks)
            .into_iter()
            .partition::<Vec<_>, _>(|pending| {
                pending.open_runs == 0 && (all || Some(pending.block.base_addr()) != carving)
            });
        if all {
            self.carving_block = None;
        }
        self.blocks = kept;
        let blocks = blocks
            .into_iter()
            .map(|pending| AdoptedBlock {
                block: pending.block,
                handed: pending.handed,
                unused: pending.unused,
            })
            .collect();
        Adoption {
            blocks,
            floats: self.floats.drain(all),
            strings: self.strings.drain(all),
            vectors: self.vectors.drain(all),
            boxed: std::mem::take(&mut self.boxed),
            charges: std::mem::take(&mut self.charges),
        }
    }
}

/// The owner allocator a heap shares with its owner threads.
pub(crate) struct OwnerPages {
    cold: parking_lot::Mutex<AllocatorCold>,
    sizes: OwnerRunSizes,
    refills: AtomicU64,
    lock_wait_ns: AtomicU64,
    adoptions: AtomicU64,
}

impl OwnerPages {
    pub(crate) fn new(sizes: OwnerRunSizes) -> Self {
        Self {
            cold: parking_lot::Mutex::new(AllocatorCold::new()),
            sizes,
            refills: AtomicU64::new(0),
            lock_wait_ns: AtomicU64::new(0),
            adoptions: AtomicU64::new(0),
        }
    }

    /// Take the cold mutex, counting the acquisition and any wait.
    fn lock(&self) -> parking_lot::MutexGuard<'_, AllocatorCold> {
        self.refills.fetch_add(1, Ordering::Relaxed);
        if let Some(guard) = self.cold.try_lock() {
            return guard;
        }
        let start = std::time::Instant::now();
        let guard = self.cold.lock();
        let waited = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.lock_wait_ns.fetch_add(waited, Ordering::Relaxed);
        guard
    }

    pub(crate) fn stats(&self) -> OwnerAllocStats {
        OwnerAllocStats {
            refills: self.refills.load(Ordering::Relaxed),
            lock_wait_ns: self.lock_wait_ns.load(Ordering::Relaxed),
            adoptions: self.adoptions.load(Ordering::Relaxed),
        }
    }
}
