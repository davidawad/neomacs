//! Per-owner allocation (shared-heap experiment, stage 4).
//!
//! The heap keeps ONE collector and ONE set of block and page registries. The
//! keeper (the thread holding the World's Baton) allocates exactly as before:
//! its cursors stay in `JitHeapState`. Every OTHER owner thread allocates from
//! runs it alone writes: a run of cons cells in a cons block, of float,
//! string, vector or bignum slots in a 64 KiB page. Those blocks and pages are
//! NOT in the heap's registries while owners carve them. An owner carves them
//! from blocks and pages of its own and keeps them, with its boxed
//! vector-likes (hash tables, `Box`es whose `all_objects` link waits), in its
//! own pending state: a refill (close one run, carve the next) touches
//! nothing another thread reads. When the owner closes its runs (it parks,
//! attaches or ends) it hands its pending state to [`OwnerPages`]'s cold
//! state behind one `parking_lot::Mutex`, the only time it takes that mutex.
//!
//! The keeper ADOPTS what the owners handed over: under the same mutex it
//! moves every block and page there into the heap's registries
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
//! Reuse: a sweep's empty blocks and pages are storage owners carve again.
//! As many as owners started between the last two sweeps go to the cold
//! state's spare lists ([`TaggedHeap::recycle_released_cons_blocks`],
//! [`TaggedHeap::recycle_spare_pages`]); an owner about to open a block or
//! page takes a batch of them under the cold mutex, so it pays the
//! allocator's fresh memory only when no swept storage is left.
//!
//! Everything here is `#[cfg(feature = "shared-heap")]`; without an installed
//! [`OwnerAllocState`] the allocation entry points take the keeper path.

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize};

/// How many spare blocks or pages of one class an owner takes from the cold
/// state at once.
const SPARE_BATCH: usize = 8;

/// How many cells or slots an owner takes per refill, at most: a run never
/// crosses its block or page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OwnerRunSizes {
    pub(crate) cons_cells: usize,
    pub(crate) slots: usize,
}

impl OwnerRunSizes {
    /// A whole cons block, a whole page of slots per class.
    pub(crate) const DEFAULT: Self = Self {
        cons_cells: CONS_BLOCK_SIZE,
        slots: usize::MAX,
    };
}

/// The owner allocator's counters (plain integers).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OwnerAllocStats {
    /// Runs owners carved (refills), counted when each owner closes.
    pub(crate) refills: u64,
    /// Cold-mutex acquisitions by owners (one per close).
    pub(crate) locks: u64,
    /// Nanoseconds owners waited for the cold mutex.
    pub(crate) lock_wait_ns: u64,
    /// Keeper adoptions that moved at least one block, page or object.
    pub(crate) adoptions: u64,
}

/// The size classes an owner carves runs of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
enum OwnerClass {
    Cons,
    Float,
    String,
    Vector,
    Bignum,
}

impl OwnerClass {
    const ALL: [Self; 5] = [
        Self::Cons,
        Self::Float,
        Self::String,
        Self::Vector,
        Self::Bignum,
    ];
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
    /// Storage of swept, empty pages a new page is made over first.
    spare: Vec<*mut u8>,
    /// Pages started (over spare storage or fresh) since the last recycle.
    carved: usize,
    /// How many spare storages recycling keeps: the pages owners started
    /// between the last two recycles that saw any.
    limit: usize,
}

impl<T: PagedObject> Drop for PagePool<T> {
    fn drop(&mut self) {
        for storage in self.spare.drain(..) {
            ObjectPage::<T>::free_storage(storage);
        }
    }
}

impl<T: PagedObject> PagePool<T> {
    const fn new() -> Self {
        Self {
            pages: Vec::new(),
            carving: None,
            spare: Vec::new(),
            carved: 0,
            limit: 0,
        }
    }

    /// Whether the next carve starts a page.
    fn opens_page(&mut self) -> bool {
        self.carving
            .is_none_or(|base| self.page_mut(base).page.next_index >= ObjectPage::<T>::SLOTS)
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
                let page = self
                    .spare
                    .pop()
                    .map_or_else(ObjectPage::new, ObjectPage::over_storage);
                self.carved += 1;
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

    /// Take over every page and spare storage of OTHER, whose runs are all
    /// closed.
    fn absorb(&mut self, other: &mut Self) {
        debug_assert!(other.pages.iter().all(|pending| pending.open_runs == 0));
        self.pages.append(&mut other.pages);
        other.carving = None;
        self.spare.append(&mut other.spare);
        self.carved += std::mem::take(&mut other.carved);
    }

    /// Move up to SPARE_BATCH spare storages to TO.
    fn lend_spare(&mut self, to: &mut Self) {
        let at = self.spare.len().saturating_sub(SPARE_BATCH);
        to.spare.extend(self.spare.drain(at..));
    }

    /// A sweep released pages to ARENA's spare storage: keep as many as
    /// owners started since the last recycle that saw them start any,
    /// taking ARENA's or giving the excess back to it.
    fn recycle(&mut self, arena: &mut ObjectArena<T>) {
        if self.carved > 0 {
            self.limit = std::mem::take(&mut self.carved);
        }
        if self.spare.len() > self.limit {
            arena.spare_storage.extend(self.spare.drain(self.limit..));
        }
        let take = (self.limit - self.spare.len()).min(arena.spare_storage.len());
        let at = arena.spare_storage.len() - take;
        self.spare.extend(arena.spare_storage.drain(at..));
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

/// Owner blocks, pages and boxed objects not yet adopted: one owner's own
/// pending state ([`OwnerAllocState`]), which only that owner touches, or
/// the cold state every owner hands its pending state to, only ever
/// touched under [`OwnerPages::cold`].
struct AllocatorCold {
    blocks: Vec<PendingBlock>,
    /// Base of the cons block being carved.
    carving_block: Option<usize>,
    floats: PagePool<FloatObj>,
    strings: PagePool<StringObj>,
    vectors: PagePool<VectorObj>,
    bignums: PagePool<BignumObj>,
    /// Owner-allocated boxed vector-likes awaiting their `all_objects` link.
    boxed: Vec<*mut VecLikeHeader>,
    charges: Charges,
    open_runs: usize,
    /// Storage of swept, empty cons blocks a new block is made over first.
    spare_blocks: Vec<*mut u8>,
    /// Cons blocks started (over spare storage or fresh) since the last
    /// recycle.
    carved_blocks: usize,
    /// How many spare blocks recycling keeps (as [`PagePool::limit`]).
    spare_block_limit: usize,
}

impl Drop for AllocatorCold {
    fn drop(&mut self) {
        for storage in self.spare_blocks.drain(..) {
            ConsBlock::free_storage(storage);
        }
    }
}

// SAFETY: the raw pointers name block and page storage and boxed objects this
// state owns exclusively until adoption hands them to the keeper's heap; they
// are read and written only by the owner holding the state, or under
// `OwnerPages::cold`, from whichever thread holds it. No thread-affine data
// (no Rc, no TLS-derived pointer) is held.
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
    bignums: Vec<ObjectPage<BignumObj>>,
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
            bignums: PagePool::new(),
            boxed: Vec::new(),
            charges: Charges::default(),
            open_runs: 0,
            spare_blocks: Vec::new(),
            carved_blocks: 0,
            spare_block_limit: 0,
        }
    }

    /// Take over an owner's pending state LOCAL, every run of which is
    /// closed: its blocks, pages, boxed objects and charges.
    fn absorb(&mut self, local: &mut AllocatorCold) {
        debug_assert_eq!(local.open_runs, 0, "an owner hands over closed runs");
        debug_assert!(local.blocks.iter().all(|pending| pending.open_runs == 0));
        self.blocks.append(&mut local.blocks);
        local.carving_block = None;
        self.floats.absorb(&mut local.floats);
        self.strings.absorb(&mut local.strings);
        self.vectors.absorb(&mut local.vectors);
        self.bignums.absorb(&mut local.bignums);
        self.boxed.append(&mut local.boxed);
        self.charges.add(&std::mem::take(&mut local.charges));
        self.spare_blocks.append(&mut local.spare_blocks);
        self.carved_blocks += std::mem::take(&mut local.carved_blocks);
    }

    /// Whether the next carve of CLASS starts a block or page.
    fn opens(&mut self, class: OwnerClass) -> bool {
        match class {
            OwnerClass::Cons => self.carving_block.is_none_or(|base| {
                self.block_mut(base).block.next_index as usize >= CONS_BLOCK_SIZE
            }),
            OwnerClass::Float => self.floats.opens_page(),
            OwnerClass::String => self.strings.opens_page(),
            OwnerClass::Vector => self.vectors.opens_page(),
            OwnerClass::Bignum => self.bignums.opens_page(),
        }
    }

    /// How many spare storages of CLASS this state holds.
    fn spare_len(&self, class: OwnerClass) -> usize {
        match class {
            OwnerClass::Cons => self.spare_blocks.len(),
            OwnerClass::Float => self.floats.spare.len(),
            OwnerClass::String => self.strings.spare.len(),
            OwnerClass::Vector => self.vectors.spare.len(),
            OwnerClass::Bignum => self.bignums.spare.len(),
        }
    }

    /// Move up to SPARE_BATCH spare storages of CLASS to TO.
    fn lend_spare(&mut self, class: OwnerClass, to: &mut AllocatorCold) {
        match class {
            OwnerClass::Cons => {
                let at = self.spare_blocks.len().saturating_sub(SPARE_BATCH);
                to.spare_blocks.extend(self.spare_blocks.drain(at..));
            }
            OwnerClass::Float => self.floats.lend_spare(&mut to.floats),
            OwnerClass::String => self.strings.lend_spare(&mut to.strings),
            OwnerClass::Vector => self.vectors.lend_spare(&mut to.vectors),
            OwnerClass::Bignum => self.bignums.lend_spare(&mut to.bignums),
        }
    }

    fn carve(&mut self, class: OwnerClass, sizes: OwnerRunSizes) -> OwnerRun {
        self.open_runs += 1;
        match class {
            OwnerClass::Cons => self.carve_cons(sizes.cons_cells),
            OwnerClass::Float => self.floats.carve(sizes.slots),
            OwnerClass::String => self.strings.carve(sizes.slots),
            OwnerClass::Vector => self.vectors.carve(sizes.slots),
            OwnerClass::Bignum => self.bignums.carve(sizes.slots),
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
                let mut block = self
                    .spare_blocks
                    .pop()
                    .map_or_else(ConsBlock::new, ConsBlock::over_storage);
                self.carved_blocks += 1;
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
            OwnerClass::Bignum => {
                // As `alloc_bignum_inline` charges: the object, not its limbs.
                let used = self.bignums.close(run);
                charges.allocated += used;
                charges.bytes += used * size_of::<BignumObj>();
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
            bignums: self.bignums.drain(all),
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
    locks: AtomicU64,
    lock_wait_ns: AtomicU64,
    adoptions: AtomicU64,
    /// The bytes the cold state's charges hold, not yet adopted: the
    /// keeper's pacing reads them ([`TaggedHeap::bytes_since_gc`]) between
    /// adoptions. Written under the cold mutex.
    pending_bytes: AtomicUsize,
    /// How many spare storages of each [`OwnerClass`] the cold state
    /// holds: an owner takes the cold mutex for them only when there are
    /// some. Written under the cold mutex.
    spare: [AtomicUsize; 5],
}

impl OwnerPages {
    pub(crate) fn new(sizes: OwnerRunSizes) -> Self {
        Self {
            cold: parking_lot::Mutex::new(AllocatorCold::new()),
            sizes,
            refills: AtomicU64::new(0),
            locks: AtomicU64::new(0),
            lock_wait_ns: AtomicU64::new(0),
            adoptions: AtomicU64::new(0),
            pending_bytes: AtomicUsize::new(0),
            spare: Default::default(),
        }
    }

    /// Publish COLD's spare storage counts to [`Self::spare`]; the caller
    /// holds the cold mutex COLD came from.
    fn publish_spare(&self, cold: &AllocatorCold) {
        for class in OwnerClass::ALL {
            self.spare[class as usize].store(cold.spare_len(class), Ordering::Relaxed);
        }
    }

    /// Publish COLD's charged bytes to [`Self::pending_bytes`]; the caller
    /// holds the cold mutex COLD came from. Returns them.
    fn publish_pending(&self, cold: &AllocatorCold) -> usize {
        let bytes = cold.charges.bytes;
        self.pending_bytes.store(bytes, Ordering::Relaxed);
        bytes
    }

    /// Bytes owners allocated that no adoption has charged to the heap yet.
    pub(crate) fn pending_bytes(&self) -> usize {
        self.pending_bytes.load(Ordering::Relaxed)
    }

    /// Take the cold mutex, counting the acquisition and any wait.
    fn lock(&self) -> parking_lot::MutexGuard<'_, AllocatorCold> {
        self.locks.fetch_add(1, Ordering::Relaxed);
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
            locks: self.locks.load(Ordering::Relaxed),
            lock_wait_ns: self.lock_wait_ns.load(Ordering::Relaxed),
            adoptions: self.adoptions.load(Ordering::Relaxed),
        }
    }
}

thread_local! {
    /// The owner allocation state installed on this OS thread, or null: the
    /// allocation entry points' one branch. Raw pointer only, no `Value`;
    /// [`InstalledOwner`] sets it and restores the previous value on drop.
    static OWNER_ALLOC: Cell<*mut OwnerAllocState> = const { Cell::new(std::ptr::null_mut()) };
}

/// This thread's installed owner state, null on the keeper path.
#[inline(always)]
pub(super) fn installed_owner() -> *mut OwnerAllocState {
    OWNER_ALLOC.with(Cell::get)
}

/// One owner thread's allocation state: its open runs, the blocks and pages
/// it carves them from, its uncharged counts and the boxed objects it
/// allocated since its last close.
pub(crate) struct OwnerAllocState {
    shared: Arc<OwnerPages>,
    /// The run sizes, copied from `shared` so a refill reads nothing shared.
    sizes: OwnerRunSizes,
    /// This owner's own blocks, pages and closed runs' charges, handed to
    /// `shared`'s cold state at each close.
    local: AllocatorCold,
    /// Refills since the last close.
    refills: u64,
    cons: OwnerRun,
    float: OwnerRun,
    string: OwnerRun,
    vector: OwnerRun,
    bignum: OwnerRun,
    /// The parity the last refill read; every run stamps it.
    parity: MarkParity,
    charges: Charges,
    boxed: Vec<*mut VecLikeHeader>,
    /// Bits of the heap's canonical empty strings (unibyte, multibyte) when
    /// the state was made: permanent runtime roots, copied, never traced from
    /// here. Zero when the heap had none.
    canonical_empty_bits: [usize; 2],
}

// SAFETY: the state is built by the keeper and then used by exactly one owner
// thread; its raw pointers name runs in storage the cold state keeps alive and
// boxed objects only this state links. Nothing in it is thread-affine.
unsafe impl Send for OwnerAllocState {}

/// Guard of an installed [`OwnerAllocState`]: uninstalls on drop.
#[must_use]
pub(crate) struct InstalledOwner<'state> {
    _state: std::marker::PhantomData<&'state mut OwnerAllocState>,
    previous: *mut OwnerAllocState,
}

impl Drop for InstalledOwner<'_> {
    fn drop(&mut self) {
        OWNER_ALLOC.with(|slot| slot.set(self.previous));
    }
}

impl OwnerAllocState {
    /// Route this OS thread's allocations through this state until the guard
    /// drops. The thread must also have the heap installed.
    pub(crate) fn install(&mut self) -> InstalledOwner<'_> {
        let previous = OWNER_ALLOC.with(|slot| slot.replace(std::ptr::from_mut(self)));
        InstalledOwner {
            _state: std::marker::PhantomData,
            previous,
        }
    }

    pub(crate) fn stats(&self) -> OwnerAllocStats {
        self.shared.stats()
    }

    fn run_mut(&mut self, class: OwnerClass) -> &mut OwnerRun {
        match class {
            OwnerClass::Cons => &mut self.cons,
            OwnerClass::Float => &mut self.float,
            OwnerClass::String => &mut self.string,
            OwnerClass::Vector => &mut self.vector,
            OwnerClass::Bignum => &mut self.bignum,
        }
    }

    /// Hand our charges and boxed objects to our pending state.
    fn flush(&mut self) {
        self.local.charges.add(&std::mem::take(&mut self.charges));
        self.local.boxed.append(&mut self.boxed);
    }

    /// The run of `class` is exhausted or closed: close it, carve the next
    /// one from our own blocks and pages (no lock), and read the parity it
    /// stamps. When what owners allocated since the last adoption makes a
    /// collection due, the detached thread's World defers one
    /// ([`crate::detached::defer_collection`]).
    #[cold]
    #[inline(never)]
    fn refill(&mut self, class: OwnerClass, heap: *const TaggedHeap) {
        // SAFETY: `mark_parity` and `gc_threshold` change only inside a
        // collection, and every owner has closed its runs and is parked for
        // the whole collection, so no write to them races these reads.
        let (parity, threshold) = unsafe {
            (
                std::ptr::addr_of!((*heap).mark_parity).read(),
                std::ptr::addr_of!((*heap).gc_threshold).read(),
            )
        };
        self.parity = parity;
        self.flush();
        let old = std::mem::take(self.run_mut(class));
        self.local.close(class, old);
        if self.local.opens(class)
            && self.local.spare_len(class) == 0
            && self.shared.spare[class as usize].load(Ordering::Relaxed) > 0
        {
            self.take_spare(class);
        }
        let run = self.local.carve(class, self.sizes);
        self.refills += 1;
        // What other owners handed over, and what we hold.
        let pending = self.shared.pending_bytes() + self.local.charges.bytes;
        if pending >= threshold {
            crate::detached::defer_collection(pending as u64);
        }
        if class == OwnerClass::Float {
            let mut slot = run.start;
            while slot < run.lim {
                // SAFETY: a reserved slot of a page only this run writes.
                // FULL-HEADER WRITE, as `refill_float_region` does.
                unsafe {
                    std::ptr::write(
                        slot as *mut FloatObj,
                        FloatObj {
                            header: GcHeader::new_marked(HeapObjectKind::Float, self.parity),
                            value: 0.0,
                        },
                    );
                }
                slot += FLOAT_SLOT_BYTES;
            }
        }
        *self.run_mut(class) = run;
    }

    /// Take a batch of the cold state's spare storage of CLASS: the next
    /// block or page is carved over swept memory.
    #[cold]
    fn take_spare(&mut self, class: OwnerClass) {
        let shared = Arc::clone(&self.shared);
        let mut cold = shared.lock();
        cold.lend_spare(class, &mut self.local);
        shared.publish_spare(&cold);
    }

    #[inline(always)]
    fn take(&mut self, class: OwnerClass, stride: usize, heap: *const TaggedHeap) -> usize {
        if let Some(slot) = self.run_mut(class).take(stride) {
            return slot;
        }
        self.refill(class, heap);
        self.run_mut(class)
            .take(stride)
            .expect("a fresh run has a slot")
    }

    /// `TaggedHeap::alloc_bignum_inline` on a slot of our bignum run: a
    /// FULL-header write born at the run's parity. Its alloc bit is set when
    /// the run closes; adoption puts the page in the bignum arena.
    #[inline(always)]
    pub(super) fn alloc_bignum(&mut self, heap: *const TaggedHeap, value: Integer) -> TaggedValue {
        let ptr = self.take(
            OwnerClass::Bignum,
            <BignumObj as PagedObject>::SLOT_BYTES,
            heap,
        ) as *mut BignumObj;
        // SAFETY: a never-used slot of our open run, written whole before the
        // value escapes, as the keeper path does.
        unsafe {
            std::ptr::write(
                ptr,
                BignumObj {
                    header: VecLikeHeader {
                        gc: GcHeader::new_marked(HeapObjectKind::VecLike, self.parity),
                        type_tag: VecLikeType::Bignum,
                    },
                    value,
                },
            );
            TaggedValue::from_veclike_ptr(ptr as *const VecLikeHeader)
        }
    }

    #[inline(always)]
    pub(super) fn take_cons_cell(&mut self, heap: *const TaggedHeap) -> *mut ConsCell {
        self.take(OwnerClass::Cons, size_of::<ConsCell>(), heap) as *mut ConsCell
    }

    #[inline(always)]
    pub(super) fn alloc_float(&mut self, heap: *const TaggedHeap, value: f64) -> TaggedValue {
        let ptr = self.take(OwnerClass::Float, FLOAT_SLOT_BYTES, heap) as *mut FloatObj;
        // SAFETY: a slot of our open run whose header the refill wrote.
        unsafe {
            std::ptr::addr_of_mut!((*ptr).value).write(value);
            TaggedValue::from_float_ptr(ptr)
        }
    }

    /// `TaggedHeap::alloc_string` on a slot of our string run. Its alloc bit
    /// is set when the run closes; nothing reads the bit before adoption.
    pub(super) fn alloc_string(
        &mut self,
        heap: *const TaggedHeap,
        s: crate::heap_types::LispString,
    ) -> TaggedValue {
        if s.sbytes() == 0 {
            let bits = self.canonical_empty_bits[match s.storage_kind() {
                LispStringStorageKind::Unibyte => 0,
                LispStringStorageKind::Multibyte => 1,
            }];
            if bits != 0 {
                // SAFETY: the bits of a canonical empty string, a permanent
                // runtime root of this heap.
                return unsafe { TaggedValue::from_bits(bits) };
            }
        }
        self.charges.count(MemoryUseCountSlot::Strings, 1);
        self.charges
            .count(MemoryUseCountSlot::StringChars, s.sbytes() as u64);
        let ptr = self.take(
            OwnerClass::String,
            <StringObj as PagedObject>::SLOT_BYTES,
            heap,
        ) as *mut StringObj;
        // SAFETY: a never-used slot of our open run, written whole before the
        // value escapes, as the keeper path does.
        unsafe {
            std::ptr::write(
                ptr,
                StringObj {
                    header: GcHeader::new(HeapObjectKind::String),
                    data: s,
                },
            );
            (*ptr).header.set_marked(self.parity);
            self.charges.allocated += 1;
            self.charges.bytes += TaggedHeap::string_object_bytes(&*ptr);
            TaggedValue::from_string_ptr(ptr)
        }
    }

    /// `TaggedHeap::alloc_vector` on a slot of our vector run; adoption adds
    /// it to the vector registry.
    pub(super) fn alloc_vector(
        &mut self,
        heap: *const TaggedHeap,
        items: Vec<TaggedValue>,
    ) -> TaggedValue {
        self.charges
            .count(MemoryUseCountSlot::VectorCells, items.len() as u64);
        let ptr = self.take(
            OwnerClass::Vector,
            <VectorObj as PagedObject>::SLOT_BYTES,
            heap,
        ) as *mut VectorObj;
        // SAFETY: as in `alloc_string`.
        unsafe {
            std::ptr::write(
                ptr,
                VectorObj {
                    header: VecLikeHeader::new(VecLikeType::Vector),
                    data: items.into(),
                },
            );
            (*ptr).header.gc.set_marked(self.parity);
            self.charges.allocated += 1;
            self.charges.bytes += size_of::<VectorObj>()
                .saturating_add(TaggedHeap::lisp_value_vec_storage_bytes(&(*ptr).data));
            TaggedValue::from_veclike_ptr(ptr as *const VecLikeHeader)
        }
    }

    /// `TaggedHeap::alloc_hash_table` without the `all_objects` link, which
    /// adoption makes.
    pub(super) fn alloc_hash_table(&mut self, table: crate::value::LispHashTable) -> TaggedValue {
        self.charges.count(MemoryUseCountSlot::VectorCells, 1);
        let ptr = Box::into_raw(Box::new(HashTableObj {
            header: VecLikeHeader::new(VecLikeType::HashTable),
            table,
        }));
        self.boxed.push(ptr.cast());
        self.charges.allocated += 1;
        // SAFETY: the box was just made and is ours.
        self.charges.bytes += TaggedHeap::hash_table_object_bytes(unsafe { &*ptr });
        // SAFETY: a live boxed hash table.
        unsafe { TaggedValue::from_veclike_ptr(ptr as *const VecLikeHeader) }
    }
}

/// This OS thread's installed owner state becomes STATE (null: none), for
/// a detached thread's placement (`crate::detached`), which owns it.
pub(crate) fn set_installed_owner(state: *mut OwnerAllocState) {
    OWNER_ALLOC.with(|slot| slot.set(state));
}

/// A detached thread's allocation state, made attached from HEAP and owned
/// by the thread's placement until its body ends (`crate::detached`).
pub(crate) fn new_detached_owner(heap: &mut TaggedHeap) -> std::ptr::NonNull<OwnerAllocState> {
    std::ptr::NonNull::from(Box::leak(heap.new_owner_alloc_state()))
}

/// Close every open run of `state` (cursor = limit, handed-out slots charged
/// and marked allocated, unused cells returned at adoption) and hand its
/// blocks, pages and boxed objects to the cold state, under its mutex. An
/// owner calls this when it parks for a collection.
pub(crate) fn close_owner_regions(state: &mut OwnerAllocState) {
    state.flush();
    for class in [
        OwnerClass::Cons,
        OwnerClass::Float,
        OwnerClass::String,
        OwnerClass::Vector,
        OwnerClass::Bignum,
    ] {
        let run = std::mem::take(state.run_mut(class));
        state.local.close(class, run);
    }
    let shared = Arc::clone(&state.shared);
    shared
        .refills
        .fetch_add(std::mem::take(&mut state.refills), Ordering::Relaxed);
    let mut cold = shared.lock();
    cold.absorb(&mut state.local);
    shared.publish_pending(&cold);
    shared.publish_spare(&cold);
}

/// Resume allocating after a collection. Runs reopen lazily: the next
/// allocation of each class refills, reading the collection's new parity.
pub(crate) fn reopen_owner_regions(state: &mut OwnerAllocState) {
    debug_assert!(
        [
            state.cons,
            state.float,
            state.string,
            state.vector,
            state.bignum
        ]
        .iter()
        .all(|run| run.start == 0),
        "reopen without close"
    );
}

impl TaggedHeap {
    /// The owner allocator, made on first use with `sizes`.
    fn owner_pages_with(&mut self, sizes: OwnerRunSizes) -> Arc<OwnerPages> {
        Arc::clone(
            self.owner_pages
                .get_or_insert_with(|| Arc::new(OwnerPages::new(sizes))),
        )
    }

    /// A new owner's allocation state (made by the keeper, used by the owner).
    pub(crate) fn new_owner_alloc_state(&mut self) -> Box<OwnerAllocState> {
        self.new_owner_alloc_state_with(OwnerRunSizes::DEFAULT)
    }

    /// [`Self::new_owner_alloc_state`]; `sizes` apply if this is the heap's
    /// first owner.
    pub(crate) fn new_owner_alloc_state_with(
        &mut self,
        sizes: OwnerRunSizes,
    ) -> Box<OwnerAllocState> {
        let shared = self.owner_pages_with(sizes);
        let sizes = shared.sizes;
        let empty = |kind| {
            self.canonical_empty_strings
                .get(kind)
                .map_or(0, TaggedValue::bits)
        };
        Box::new(OwnerAllocState {
            shared,
            sizes,
            local: AllocatorCold::new(),
            refills: 0,
            cons: OwnerRun::default(),
            float: OwnerRun::default(),
            string: OwnerRun::default(),
            vector: OwnerRun::default(),
            bignum: OwnerRun::default(),
            parity: self.mark_parity,
            charges: Charges::default(),
            boxed: Vec::new(),
            canonical_empty_bits: [
                empty(LispStringStorageKind::Unibyte),
                empty(LispStringStorageKind::Multibyte),
            ],
        })
    }

    /// The owner allocator's counters (zeros before the first owner).
    pub(crate) fn owner_alloc_stats(&self) -> OwnerAllocStats {
        self.owner_pages
            .as_deref()
            .map(OwnerPages::stats)
            .unwrap_or_default()
    }

    /// Blocks a sweep emptied and unregistered: owners carve as many as they
    /// started since the last recycle that saw them start any over their
    /// storage; the rest go back to the allocator. Keeper only, every
    /// owner's runs closed.
    pub(super) fn recycle_released_cons_blocks(&mut self, released: Vec<ConsBlock>) {
        let Some(shared) = self.owner_pages.as_deref() else {
            return;
        };
        let mut cold = shared.cold.lock();
        if cold.carved_blocks > 0 {
            cold.spare_block_limit = std::mem::take(&mut cold.carved_blocks);
        }
        let limit = cold.spare_block_limit;
        for block in released {
            if cold.spare_blocks.len() < limit {
                cold.spare_blocks.push(block.into_storage());
            }
        }
        while cold.spare_blocks.len() > limit {
            ConsBlock::free_storage(cold.spare_blocks.pop().expect("above the limit"));
        }
        shared.publish_spare(&cold);
    }

    /// A sweep released empty pages to each arena's spare storage: owners
    /// carve some of it ([`PagePool::recycle`]). Keeper only, every owner's
    /// runs closed.
    pub(super) fn recycle_spare_pages(&mut self) {
        let Some(shared) = self.owner_pages.clone() else {
            return;
        };
        let mut cold = shared.cold.lock();
        cold.floats.recycle(&mut self.float_arena);
        cold.strings.recycle(&mut self.string_arena);
        cold.vectors.recycle(&mut self.vector_arena);
        cold.bignums.recycle(&mut self.bignum_arena);
        shared.publish_spare(&cold);
    }

    /// Move every owner block, page and boxed object no open run still
    /// writes into this heap's registries, and fold the owners' charges into
    /// the consing counters. Keeper only (`&mut self`).
    pub(crate) fn adopt_owner_pages(&mut self) {
        let Some(shared) = self.owner_pages.clone() else {
            return;
        };
        let adoption = {
            let mut cold = shared.cold.lock();
            let adoption = cold.drain();
            shared.publish_pending(&cold);
            adoption
        };
        let Adoption {
            blocks,
            floats,
            strings,
            vectors,
            bignums,
            boxed,
            charges,
        } = adoption;
        let moved = blocks.len()
            + floats.len()
            + strings.len()
            + vectors.len()
            + bignums.len()
            + boxed.len();
        for pending in blocks {
            self.adopt_owner_cons_block(pending);
        }
        for page in floats {
            self.float_arena.adopt_page(page);
        }
        for page in strings {
            self.string_arena.adopt_page(page);
        }
        for page in vectors {
            let base = page.base_addr();
            let index = self.vector_arena.adopt_page(page);
            debug_assert_eq!(self.vector_arena.pages[index].base_addr(), base);
            for slot in self.vector_arena.pages[index].allocated_slots() {
                let registered = self.vector_object_addrs.insert(slot as usize);
                debug_assert!(registered, "owner vector registered twice");
            }
        }
        for page in bignums {
            self.bignum_arena.adopt_page(page);
        }
        for header in boxed {
            self.link_veclike(header);
        }
        for (slot, n) in charges.counts.iter().enumerate() {
            let counts = &mut self.current_mutator_gc_mut().memory_use_counts;
            counts[slot] = counts[slot].wrapping_add(*n);
        }
        self.current_mutator_gc_mut().allocated_count += charges.allocated;
        self.cons_live_count += charges.cons_live;
        self.note_allocation_bytes(charges.bytes);
        if moved > 0 {
            shared.adoptions.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Test-support detached section for crates above this one: run
/// `body(i)` for `i` in `0..threads` on that many owner OS threads at once,
/// each with the calling thread's heap and its own owner state installed
/// (installation is serialized, standing in for an attach), then close every
/// state, reinstall the heap on the caller (the keeper) and adopt. Returns
/// the refills (runs carved) and the cold mutex's contended wait nanoseconds
/// the section added. The caller allocates nothing while the section runs.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn run_detached_owners_for_test(threads: usize, body: impl Fn(usize) + Sync) -> (u64, u64) {
    let root = std::ptr::NonNull::new(TAGGED_HEAP.with(Cell::get)).expect("a heap is installed");
    let (mut states, before) = {
        // SAFETY: the installed root of the caller's live heap; the loan ends
        // before any owner thread starts.
        let heap = unsafe { &mut *root.as_ptr() };
        let states: Vec<_> = (0..threads).map(|_| heap.new_owner_alloc_state()).collect();
        (states, heap.owner_alloc_stats())
    };
    let address = root.as_ptr() as usize;
    let baton = std::sync::Mutex::new(());
    let start = std::sync::Barrier::new(threads);
    std::thread::scope(|scope| {
        for (index, state) in states.iter_mut().enumerate() {
            let (baton, start, body) = (&baton, &start, &body);
            scope.spawn(move || {
                {
                    let _attached = baton.lock().expect("the stand-in Baton");
                    let root =
                        std::ptr::NonNull::new(address as *mut TaggedHeap).expect("a heap root");
                    set_tagged_heap_root(root);
                }
                {
                    let _installed = state.install();
                    start.wait();
                    body(index);
                }
                close_owner_regions(state);
            });
        }
    });
    set_tagged_heap_root(root);
    // SAFETY: every owner thread has joined; the keeper's loan is exclusive.
    let heap = unsafe { &mut *root.as_ptr() };
    heap.close_alloc_regions();
    let after = heap.owner_alloc_stats();
    (
        after.refills - before.refills,
        after.lock_wait_ns - before.lock_wait_ns,
    )
}
