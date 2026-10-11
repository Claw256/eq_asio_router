//! Trailing-edge debounce driven by explicit timestamps (pure, testable).

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct Debounce {
    delay: Duration,
    last_edit: Option<Instant>,
}

impl Debounce {
    pub fn new(delay: Duration) -> Self {
        Self { delay, last_edit: None }
    }

    /// Record an edit at `now`.
    pub fn mark(&mut self, now: Instant) {
        self.last_edit = Some(now);
    }

    pub fn clear(&mut self) {
        self.last_edit = None;
    }

    pub fn is_pending(&self) -> bool {
        self.last_edit.is_some()
    }

    /// Returns true exactly once when the quiet period has elapsed.
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.last_edit {
            Some(t) if now.saturating_duration_since(t) >= self.delay => {
                self.last_edit = None;
                true
            }
            _ => false,
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_once_after_quiet_period() {
        let t0 = Instant::now();
        let mut d = Debounce::new(Duration::from_millis(250));
        assert!(!d.poll(t0));
        d.mark(t0);
        assert!(!d.poll(t0 + Duration::from_millis(100)));
        d.mark(t0 + Duration::from_millis(200)); // restarts the window
        assert!(!d.poll(t0 + Duration::from_millis(400)));
        assert!(d.poll(t0 + Duration::from_millis(450)));
        assert!(!d.poll(t0 + Duration::from_millis(900)));
        assert!(!d.is_pending());
    }

}
