//! Nonces for `/exchange` actions.
//!
//! A nonce is a millisecond timestamp. The node keeps the 100 highest nonces per signer and
//! accepts a new one inside `[block time - 2 days, block time + 1 day]` if it is not in that
//! set, so a strictly increasing clock value is always fresh.

use std::sync::atomic::{AtomicU64, Ordering};

/// Hands out strictly increasing nonces for one signer.
#[derive(Debug, Default)]
pub struct NonceManager {
    last: AtomicU64,
}

impl NonceManager {
    /// Returns `now_ms`, or the last nonce plus one when the clock did not move forward.
    pub fn next(&self, now_ms: u64) -> u64 {
        let mut nonce = now_ms;
        self.last
            .update(Ordering::SeqCst, Ordering::SeqCst, |last| {
                nonce = now_ms.max(last.saturating_add(1));
                nonce
            });
        nonce
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, sync::Arc};

    use super::*;

    #[test]
    fn nonces_increase_when_the_clock_stalls_or_goes_back() {
        let nonces = NonceManager::default();
        assert_eq!(nonces.next(1_000), 1_000);
        assert_eq!(nonces.next(1_000), 1_001);
        assert_eq!(nonces.next(900), 1_002);
        assert_eq!(nonces.next(5_000), 5_000);
    }

    #[test]
    fn concurrent_callers_never_share_a_nonce() {
        let nonces = Arc::new(NonceManager::default());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let nonces = Arc::clone(&nonces);
                std::thread::spawn(move || (0..500).map(|_| nonces.next(42)).collect::<Vec<_>>())
            })
            .collect();
        let all: Vec<u64> = threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(all.iter().collect::<HashSet<_>>().len(), 4_000);
    }
}
