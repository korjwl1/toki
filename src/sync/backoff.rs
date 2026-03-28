use std::time::Duration;

/// Exponential backoff with configurable cap.
///
/// Sequence: 0s (immediate) → 2s → 4s → 8s → 16s → 32s → 64s → 128s → 256s → 300s → 300s → …
pub struct Backoff {
    attempt: u32,
    cap: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    pub fn new() -> Self {
        Self {
            attempt: 0,
            cap: Duration::from_secs(300),
        }
    }

    /// Return the delay for the current attempt, then advance the counter.
    pub fn next_delay(&mut self) -> Duration {
        let delay = if self.attempt == 0 {
            Duration::ZERO
        } else {
            let secs = 2u64.saturating_pow(self.attempt - 1).min(self.cap.as_secs());
            Duration::from_secs(secs)
        };
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_sequence() {
        let mut b = Backoff::new();
        assert_eq!(b.next_delay(), Duration::ZERO);           // attempt 0: immediate
        assert_eq!(b.next_delay(), Duration::from_secs(1));   // 2^0 = 1
        assert_eq!(b.next_delay(), Duration::from_secs(2));   // 2^1 = 2
        assert_eq!(b.next_delay(), Duration::from_secs(4));   // 2^2 = 4
        assert_eq!(b.next_delay(), Duration::from_secs(8));   // 2^3 = 8
    }

    #[test]
    fn test_backoff_cap() {
        let mut b = Backoff::new();
        // Run until capped
        let mut last = Duration::ZERO;
        for _ in 0..20 {
            last = b.next_delay();
        }
        assert!(last <= Duration::from_secs(300), "should be capped at 300s, got {:?}", last);
        // Further calls should stay at cap
        assert_eq!(b.next_delay(), Duration::from_secs(300));
    }

    #[test]
    fn test_backoff_reset() {
        let mut b = Backoff::new();
        b.next_delay(); // immediate
        b.next_delay(); // 1s
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO); // back to immediate
    }
}
