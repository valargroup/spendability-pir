use std::collections::VecDeque;

/// Result of pushing a new block to the chain tracker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainAction {
    /// Block extends the current chain normally.
    Extend,
    /// A reorg was detected. The consumer must roll back to the given height
    /// (exclusive -- blocks at and above this height are orphaned).
    Reorg { rollback_to: u64 },
}

/// Bounded-window chain tracker for reorg detection.
///
/// Keeps the last N `(height, hash)` pairs and checks that each new block's
/// `prev_hash` links to the current tip. If it doesn't, walks back to find
/// the fork point.
pub struct ChainTracker {
    /// Ring of (height, hash) entries, oldest first.
    chain: VecDeque<(u64, [u8; 32])>,
    max_depth: usize,
}

impl ChainTracker {
    pub fn new(max_depth: usize) -> Self {
        Self {
            chain: VecDeque::with_capacity(max_depth + 1),
            max_depth,
        }
    }

    /// Reconstruct from a known tip (e.g. after snapshot restore).
    pub fn with_tip(height: u64, hash: [u8; 32], max_depth: usize) -> Self {
        let mut tracker = Self::new(max_depth);
        tracker.chain.push_back((height, hash));
        tracker
    }

    pub fn tip(&self) -> Option<(u64, [u8; 32])> {
        self.chain.back().copied()
    }

    /// Push a new block. Returns whether it extends the chain or triggers a reorg.
    pub fn push_block(&mut self, height: u64, hash: [u8; 32], prev_hash: [u8; 32]) -> ChainAction {
        if let Some((tip_height, tip_hash)) = self.chain.back() {
            if prev_hash == *tip_hash && height == tip_height + 1 {
                // Normal extension
                self.chain.push_back((height, hash));
                if self.chain.len() > self.max_depth {
                    self.chain.pop_front();
                }
                return ChainAction::Extend;
            }

            // Reorg detected: walk back to find fork point
            let fork_height = self.find_fork_point(&prev_hash);
            self.truncate_to(fork_height);
            self.chain.push_back((height, hash));
            return ChainAction::Reorg {
                rollback_to: fork_height,
            };
        }

        // First block — always extends
        self.chain.push_back((height, hash));
        ChainAction::Extend
    }

    /// Remove all entries at heights > fork_height.
    pub fn truncate_to(&mut self, fork_height: u64) {
        while let Some((h, _)) = self.chain.back() {
            if *h > fork_height {
                self.chain.pop_back();
            } else {
                break;
            }
        }
    }

    /// Find the height of the block whose hash matches `target_hash`.
    /// Returns 0 if the hash is not found in our window (deep reorg beyond our tracking).
    fn find_fork_point(&self, target_hash: &[u8; 32]) -> u64 {
        for (height, hash) in self.chain.iter().rev() {
            if hash == target_hash {
                return *height;
            }
        }
        // Hash not found in our window. The reorg is deeper than we track.
        // Return the height just before our earliest tracked block, or 0.
        self.chain
            .front()
            .map(|(h, _)| h.saturating_sub(1))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(n: u8) -> [u8; 32] {
        [n; 32]
    }

    #[test]
    fn test_chain_tracker_extend() {
        let mut tracker = ChainTracker::new(100);

        assert_eq!(tracker.push_block(1, hash(1), hash(0)), ChainAction::Extend);
        assert_eq!(tracker.push_block(2, hash(2), hash(1)), ChainAction::Extend);
        assert_eq!(tracker.push_block(3, hash(3), hash(2)), ChainAction::Extend);

        assert_eq!(tracker.tip(), Some((3, hash(3))));
    }

    #[test]
    fn test_chain_tracker_reorg() {
        let mut tracker = ChainTracker::new(100);

        // Build chain: 1 -> 2 -> 3
        tracker.push_block(1, hash(1), hash(0));
        tracker.push_block(2, hash(2), hash(1));
        tracker.push_block(3, hash(3), hash(2));

        // Reorg: new block at height 3 with prev_hash = hash(2) but different hash
        let result = tracker.push_block(3, hash(33), hash(2));
        assert_eq!(result, ChainAction::Reorg { rollback_to: 2 });
        assert_eq!(tracker.tip(), Some((3, hash(33))));
    }

    #[test]
    fn test_chain_tracker_deep_reorg() {
        let mut tracker = ChainTracker::new(100);

        // Build chain: 1 -> 2 -> 3 -> 4
        tracker.push_block(1, hash(1), hash(0));
        tracker.push_block(2, hash(2), hash(1));
        tracker.push_block(3, hash(3), hash(2));
        tracker.push_block(4, hash(4), hash(3));

        // Reorg 2 blocks deep: new block at height 3 with prev_hash = hash(2)
        let result = tracker.push_block(3, hash(33), hash(2));
        assert_eq!(result, ChainAction::Reorg { rollback_to: 2 });
        assert_eq!(tracker.tip(), Some((3, hash(33))));
    }

    #[test]
    fn test_chain_tracker_window_eviction() {
        let mut tracker = ChainTracker::new(5);

        for i in 1u8..=10 {
            tracker.push_block(i as u64, hash(i), hash(i - 1));
        }

        // Window should only keep the last 5 blocks
        assert_eq!(tracker.chain.len(), 5);
        assert_eq!(tracker.tip(), Some((10, hash(10))));
    }

    #[test]
    fn test_chain_tracker_with_tip() {
        let mut tracker = ChainTracker::with_tip(100, hash(100), 50);

        let result = tracker.push_block(101, hash(101), hash(100));
        assert_eq!(result, ChainAction::Extend);
    }

    #[test]
    fn test_chain_tracker_reorg_at_tip() {
        let mut tracker = ChainTracker::new(100);
        tracker.push_block(1, hash(1), hash(0));
        tracker.push_block(2, hash(2), hash(1));

        // Replace tip: new block 2' with prev_hash = hash(1)
        let result = tracker.push_block(2, hash(22), hash(1));
        assert_eq!(result, ChainAction::Reorg { rollback_to: 1 });
        assert_eq!(tracker.tip(), Some((2, hash(22))));
    }
}
