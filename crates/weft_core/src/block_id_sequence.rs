//! Block-ID allocation: per-terminal sequences plus the shared hi/lo pool.
//!
//! T14 (PLAN_v11217 §3.9): the shared allocator used to be a bare
//! `Arc<AtomicU64>` seeded from `MAX(id)+1` on every open. Deleting the
//! newest row (prune) then rolled the seed back and the next command could
//! REUSE a deleted id — recovery snapshots referencing that id would hydrate
//! the WRONG block. The fix is a persistent monotonic counter in the DB's
//! `meta` table driven by a hi/lo reservation protocol:
//!
//! - `BlockStore::open` grabs a reserved segment `[lo, hi)` atomically
//!   (`UPDATE meta SET v = v + RESERVE`, read back) inside one transaction,
//!   so concurrent instances never overlap segments. A crash discards the
//!   unused reservation (an id hole — harmless) and never rolls back.
//! - Allocation inside the reservation stays lock-free
//!   (`next.fetch_add`). When the counter reaches `hi` (exhaustion) or
//!   `observe` pushes it beyond, the pool re-grabs by invoking the refill
//!   closure installed by `BlockStore` (which owns DB access).
//! - A failed refill (DB busy) is tolerated: the counter keeps running
//!   past `hi` — monotonicity is what matters for id safety, and the next
//!   allocation retries the refill.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Ids reserved from the persistent counter per grab (PLAN_v11217 §3.9:
/// RESERVE = 4096 — one extension per ~4096 commands).
pub const ID_RESERVE: u64 = 4096;

/// Re-grabs a reservation segment and returns the new exclusive bound
/// (`hi`). Implemented by [`crate::persistence::BlockStore`] with DB access;
/// `None` = the grab failed (busy / IO) and the caller may overshoot.
pub type ReservationRefill = dyn Fn() -> Option<u64> + Send + Sync;

/// Shared hi/lo reservation state handed to every tab's
/// [`BlockIdSequence`]. `next` is the lock-free allocation cursor; `hi` is
/// the exclusive bound of the segment this process owns.
pub struct BlockIdPool {
    next: AtomicU64,
    hi: AtomicU64,
    /// Installed once by `BlockStore::open`; `Mutex` only touches the rare
    /// refill path (once per `ID_RESERVE` allocations), never `allocate`.
    refill: Mutex<Option<Box<ReservationRefill>>>,
}

impl BlockIdPool {
    /// Create a pool for the reserved segment `[next, hi)`. Production
    /// callers get their pool from `BlockStore::open` (which seeds it from
    /// the persistent counter); direct construction is for tests.
    pub fn new(next: u64, hi: u64) -> Self {
        Self {
            next: AtomicU64::new(next),
            hi: AtomicU64::new(hi.max(next)),
            refill: Mutex::new(None),
        }
    }

    /// Install the reservation re-grab closure (called with the DB path by
    /// `BlockStore::open`).
    pub(crate) fn install_refill(&self, refill: Box<ReservationRefill>) {
        *self
            .refill
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(refill);
    }

    /// Current allocation cursor (next id to be handed out). Exposed for
    /// tests and the seeding assertion.
    pub fn next(&self) -> u64 {
        self.next.load(Ordering::Relaxed)
    }

    /// Exclusive upper bound of the currently reserved segment.
    pub fn hi(&self) -> u64 {
        self.hi.load(Ordering::Relaxed)
    }

    /// Lock-free allocation inside the reservation; re-grabs when the
    /// cursor reaches the exclusive bound.
    pub(crate) fn allocate(&self) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.extend_if_beyond(id);
        id
    }

    /// Raise the cursor to cover an observed id (loaded history / another
    /// tab's allocation) and re-grab when it crosses the bound.
    pub(crate) fn observe(&self, observed_next: u64) {
        let prev = self.next.fetch_max(observed_next, Ordering::Relaxed);
        self.extend_if_beyond(prev.max(observed_next));
    }

    /// Rare path: re-grab reservations until the bound covers `needed`.
    /// Double-checked under the mutex so concurrent allocators extend once.
    fn extend_if_beyond(&self, needed: u64) {
        if needed < self.hi.load(Ordering::Relaxed) {
            return;
        }
        let refill = self
            .refill
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another thread may have extended while we waited for the mutex.
        while needed >= self.hi.load(Ordering::Relaxed) {
            match refill.as_deref().and_then(|grab| grab()) {
                Some(new_hi) => {
                    self.hi.fetch_max(new_hi, Ordering::Relaxed);
                    if new_hi > needed {
                        break;
                    }
                    // Observed id far above one reservation (hydration of a
                    // huge history): keep grabbing until covered.
                }
                None => break, // DB busy/unavailable: overshoot, retry later
            }
        }
    }
}

/// Manual `Debug`: the refill closure is not `Debug`, so only the cursor
/// and bound print.
impl fmt::Debug for BlockIdPool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockIdPool")
            .field("next", &self.next.load(Ordering::Relaxed))
            .field("hi", &self.hi.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Local IDs for isolated terminals, optionally backed by a shared
/// [`BlockIdPool`] when several tabs persist into one SQLite primary-key
/// namespace.
#[derive(Debug)]
pub(crate) struct BlockIdSequence {
    local_next: u64,
    shared: Option<Arc<BlockIdPool>>,
}

impl BlockIdSequence {
    pub(crate) fn new() -> Self {
        Self {
            local_next: 1,
            shared: None,
        }
    }

    pub(crate) fn observe(&mut self, id: u64) {
        self.local_next = self.local_next.max(id.saturating_add(1));
        if let Some(pool) = &self.shared {
            pool.observe(self.local_next);
        }
    }

    pub(crate) fn share(&mut self, pool: Arc<BlockIdPool>) {
        pool.observe(self.local_next);
        self.shared = Some(pool);
    }

    pub(crate) fn allocate(&mut self) -> u64 {
        let id = match &self.shared {
            Some(pool) => pool.allocate(),
            None => self.local_next,
        };
        self.local_next = self.local_next.max(id.saturating_add(1));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn shared_sequences_diverge_after_observing_the_same_history() {
        let shared = Arc::new(BlockIdPool::new(8, 8 + ID_RESERVE));
        let mut a = BlockIdSequence::new();
        let mut b = BlockIdSequence::new();
        a.observe(7);
        b.observe(7);
        a.share(shared.clone());
        b.share(shared);

        assert_eq!(a.allocate(), 8);
        assert_eq!(b.allocate(), 9);
    }

    #[test]
    fn pool_hands_out_ids_within_the_reservation_lock_free() {
        let pool = Arc::new(BlockIdPool::new(100, 200));
        let mut seq = BlockIdSequence::new();
        seq.share(pool.clone());
        assert_eq!(seq.allocate(), 100);
        assert_eq!(seq.allocate(), 101);
        assert_eq!(pool.next(), 102);
        assert_eq!(pool.hi(), 200);
    }

    /// Exhausting the reservation re-grabs without moving the cursor
    /// backwards: ids stay monotonically increasing across the boundary.
    #[test]
    fn exhausted_reservation_regrabs_and_keeps_ids_monotonic() {
        let pool = Arc::new(BlockIdPool::new(100, 102));
        let grabs = Arc::new(AtomicU64::new(0));
        let grabs_ref = grabs.clone();
        pool.install_refill(Box::new(move || {
            grabs_ref.fetch_add(1, Ordering::SeqCst);
            Some(102 + 4096)
        }));
        let mut seq = BlockIdSequence::new();
        seq.share(pool.clone());
        assert_eq!(seq.allocate(), 100);
        assert_eq!(seq.allocate(), 101);
        assert_eq!(seq.allocate(), 102, "reaches the bound and re-grabs");
        assert_eq!(grabs.load(Ordering::SeqCst), 1);
        assert_eq!(pool.hi(), 102 + 4096);
        assert_eq!(seq.allocate(), 103, "cursor never jumps backwards");
    }

    /// T14 acceptance: observe() pushing the shared cursor beyond the
    /// reservation must re-grab until the bound covers it, so subsequent
    /// allocations stay inside an exclusively-owned segment.
    #[test]
    fn observe_beyond_reservation_regrabs_until_covered() {
        let pool = Arc::new(BlockIdPool::new(1, 1 + ID_RESERVE));
        // Mimic the DB counter: every grab advances the bound by one
        // RESERVE segment (a CONSTANT return here would spin the extend
        // loop forever — the DB counter only ever moves forward).
        let counter = Arc::new(AtomicU64::new(1 + ID_RESERVE));
        let counter_ref = counter.clone();
        pool.install_refill(Box::new(move || {
            let prev = counter_ref.fetch_add(ID_RESERVE, Ordering::SeqCst);
            Some(prev + ID_RESERVE)
        }));
        let mut seq = BlockIdSequence::new();
        seq.share(pool.clone());

        let far = 1 + ID_RESERVE + 10_000; // far beyond one reservation
        seq.observe(far);
        assert!(pool.hi() > far, "bound must cover the observed id");

        let id = seq.allocate();
        assert_eq!(id, far + 1);
        assert!(id < pool.hi(), "allocation stays inside the reservation");
    }

    /// A failing refill must not stall allocation: the cursor overshoots
    /// (id hole, monotonic) and a later refill re-covers the bound.
    #[test]
    fn failed_refill_overshoots_then_recovers() {
        let pool = Arc::new(BlockIdPool::new(1, 3));
        let fail_first = Arc::new(AtomicBool::new(true));
        let fail_ref = fail_first.clone();
        pool.install_refill(Box::new(move || {
            if fail_ref.swap(false, Ordering::SeqCst) {
                None // simulate busy DB
            } else {
                Some(100)
            }
        }));
        let mut seq = BlockIdSequence::new();
        seq.share(pool.clone());
        assert_eq!(seq.allocate(), 1);
        assert_eq!(seq.allocate(), 2);
        assert_eq!(seq.allocate(), 3, "overshoots the bound on failed refill");
        assert_eq!(seq.allocate(), 4);
        assert_eq!(pool.hi(), 100, "later refill re-covers the bound");
    }
}
