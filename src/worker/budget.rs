use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Tracks cumulative token usage and fires a cancellation token when a budget limit is exceeded.
#[derive(Clone)]
pub struct BudgetTracker {
    inner: Arc<BudgetInner>,
}

struct BudgetInner {
    total_tokens: AtomicU32,
    limit: u32,
    token: CancellationToken,
}

impl BudgetTracker {
    /// Creates a new budget tracker with the given limit and cancellation token.
    pub fn new(limit: u32, token: CancellationToken) -> Self {
        Self {
            inner: Arc::new(BudgetInner {
                total_tokens: AtomicU32::new(0),
                limit,
                token,
            }),
        }
    }

    /// Records token usage. If the cumulative total exceeds the limit,
    /// the cancellation token is fired (only once).
    pub fn record(&self, tokens: u32) {
        let prev = self.inner.total_tokens.fetch_add(tokens, Ordering::Relaxed);
        let new_total = prev + tokens;
        if new_total >= self.inner.limit {
            self.inner.token.cancel();
        }
    }

    /// Returns the current cumulative token count.
    pub fn total_tokens(&self) -> u32 {
        self.inner.total_tokens.load(Ordering::Relaxed)
    }

    /// Returns whether the budget has been exceeded.
    pub fn is_exceeded(&self) -> bool {
        self.total_tokens() >= self.inner.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_under_limit() {
        let token = CancellationToken::new();
        let tracker = BudgetTracker::new(100, token.clone());
        tracker.record(30);
        tracker.record(40);
        assert_eq!(tracker.total_tokens(), 70);
        assert!(!tracker.is_exceeded());
        assert!(!token.is_cancelled());
    }

    #[test]
    fn budget_exceeds_limit_fires_token() {
        let token = CancellationToken::new();
        let tracker = BudgetTracker::new(100, token.clone());
        tracker.record(60);
        assert!(!token.is_cancelled());
        tracker.record(50); // total = 110, exceeds 100
        assert!(token.is_cancelled());
        assert!(tracker.is_exceeded());
    }

    #[test]
    fn budget_fires_once() {
        let token = CancellationToken::new();
        let tracker = BudgetTracker::new(100, token.clone());
        tracker.record(100); // exactly at limit
        assert!(token.is_cancelled());
        // Recording more doesn't panic
        tracker.record(50);
        assert_eq!(tracker.total_tokens(), 150);
        assert!(tracker.is_exceeded());
    }
}
