//! Cooperative cancellation shared by the engine (session stop, retry sleeps, look-ahead waits) and the client.
//!
//! A token starts live and can be cancelled once (idempotent). Cancelling wakes every thread sleeping in
//! [`CancelToken::sleep`] at once. In-flight blocking HTTP requests are **not** interrupted (blocking `ureq`); the
//! client checks the token before each request it sends.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

#[derive(Default)]
struct Inner {
    cancelled: Mutex<bool>,
    wake: Condvar,
}

/// A cancellation token. Clones share state.
#[derive(Clone, Default)]
pub struct CancelToken {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken").field("cancelled", &self.is_cancelled()).finish()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels and wakes every waiter.
    pub fn cancel(&self) {
        let mut cancelled = self.inner.cancelled.lock();
        *cancelled = true;
        self.inner.wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        *self.inner.cancelled.lock()
    }

    /// Sleeps up to `d`; returns `false` if cancelled (early).
    pub fn sleep(&self, d: Duration) -> bool {
        let deadline = Instant::now().checked_add(d);
        let mut cancelled = self.inner.cancelled.lock();
        loop {
            if *cancelled {
                return false;
            }
            match deadline {
                Some(at) => {
                    if Instant::now() >= at {
                        return true;
                    }
                    // Spurious wake-ups and timeouts both loop back to the checks above.
                    let _ = self.inner.wake.wait_until(&mut cancelled, at);
                }
                // A duration too large to represent as an instant: wait for cancellation only.
                None => self.inner.wake.wait(&mut cancelled),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_completes_when_not_cancelled() {
        let t = CancelToken::new();
        let start = Instant::now();
        assert!(t.sleep(Duration::from_millis(30)));
        assert!(start.elapsed() >= Duration::from_millis(30));
        assert!(!t.is_cancelled());
    }

    #[test]
    fn cancel_wakes_a_sleeper_early_and_is_shared_by_clones() {
        let t = CancelToken::new();
        let clone = t.clone();
        let start = Instant::now();
        let h = std::thread::spawn(move || clone.sleep(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(20));
        t.cancel();
        assert!(!h.join().unwrap());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(t.is_cancelled());
    }

    #[test]
    fn sleeping_on_a_cancelled_token_returns_at_once() {
        let t = CancelToken::new();
        t.cancel();
        t.cancel(); // idempotent
        let start = Instant::now();
        assert!(!t.sleep(Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(!t.sleep(Duration::ZERO));
    }

    #[test]
    fn zero_sleep_on_a_live_token_is_true() {
        assert!(CancelToken::new().sleep(Duration::ZERO));
    }
}
