use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Local IDs for isolated terminals, optionally backed by a shared sequence
/// when several tabs persist into one SQLite primary-key namespace.
#[derive(Debug)]
pub(crate) struct BlockIdSequence {
    local_next: u64,
    shared_next: Option<Arc<AtomicU64>>,
}

impl BlockIdSequence {
    pub(crate) fn new() -> Self {
        Self {
            local_next: 1,
            shared_next: None,
        }
    }

    pub(crate) fn observe(&mut self, id: u64) {
        self.local_next = self.local_next.max(id.saturating_add(1));
        if let Some(next) = &self.shared_next {
            next.fetch_max(self.local_next, Ordering::Relaxed);
        }
    }

    pub(crate) fn share(&mut self, next: Arc<AtomicU64>) {
        next.fetch_max(self.local_next, Ordering::Relaxed);
        self.shared_next = Some(next);
    }

    pub(crate) fn allocate(&mut self) -> u64 {
        let id = match &self.shared_next {
            Some(next) => next.fetch_add(1, Ordering::Relaxed),
            None => self.local_next,
        };
        self.local_next = self.local_next.max(id.saturating_add(1));
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_sequences_diverge_after_observing_the_same_history() {
        let shared = Arc::new(AtomicU64::new(1));
        let mut a = BlockIdSequence::new();
        let mut b = BlockIdSequence::new();
        a.observe(7);
        b.observe(7);
        a.share(shared.clone());
        b.share(shared);

        assert_eq!(a.allocate(), 8);
        assert_eq!(b.allocate(), 9);
    }
}
