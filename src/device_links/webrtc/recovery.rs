use std::time::Duration;

/// Bounded retry schedule for rebuilding a failed WebRTC peer.  A rebuild
/// creates a new signed attempt and wire generation while retaining the
/// logical paired device session and durable transfer checkpoints.
pub const RECOVERY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryState {
    attempt: usize,
    scheduled: bool,
}

impl RecoveryState {
    /// Claims one recovery worker. Repeated failure callbacks while that
    /// worker is pending are coalesced.
    pub fn claim(&mut self) -> Option<Duration> {
        if self.scheduled {
            return None;
        }
        let delay = RECOVERY_DELAYS[self.attempt.min(RECOVERY_DELAYS.len() - 1)];
        self.attempt = self.attempt.saturating_add(1);
        self.scheduled = true;
        Some(delay)
    }

    pub fn release(&mut self) {
        self.scheduled = false;
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
        self.scheduled = false;
    }

    pub fn attempts(&self) -> usize {
        self.attempt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_is_bounded_and_duplicate_workers_are_coalesced() {
        let mut state = RecoveryState::default();
        assert_eq!(state.claim(), Some(Duration::from_secs(1)));
        assert_eq!(state.claim(), None);
        state.release();
        assert_eq!(state.claim(), Some(Duration::from_secs(2)));
        state.release();
        for expected in [4, 8, 16, 30, 30] {
            assert_eq!(state.claim(), Some(Duration::from_secs(expected)));
            state.release();
        }
    }

    #[test]
    fn successful_handover_resets_the_schedule() {
        let mut state = RecoveryState::default();
        state.claim();
        state.release();
        state.claim();
        state.reset();
        assert_eq!(state.attempts(), 0);
        assert_eq!(state.claim(), Some(Duration::from_secs(1)));
    }
}
