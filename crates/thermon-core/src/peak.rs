//! Rolling maximum over a time window.
//!
//! The throttle heuristic compares the current clock with a recent peak. An
//! all-time peak is usually one short single-core boost burst, which makes
//! any sustained all-core load look like a big clock drop; a peak over the
//! last few minutes reflects what the CPU has actually been sustaining.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the throttle heuristic looks for the peak clock.
pub const THROTTLE_PEAK_WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Debug)]
pub struct RecentPeak {
    window: Duration,
    /// Monotonic deque: values strictly decrease from front to back, so the
    /// front is always the window's maximum.
    samples: VecDeque<(Instant, u64)>,
}

impl RecentPeak {
    pub fn new(window: Duration) -> Self {
        RecentPeak {
            window,
            samples: VecDeque::new(),
        }
    }

    /// Add a sample taken at `at` and return the maximum over the window.
    pub fn push(&mut self, at: Instant, value: u64) -> u64 {
        while self.samples.back().is_some_and(|(_, v)| *v <= value) {
            self.samples.pop_back();
        }
        self.samples.push_back((at, value));
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| at.saturating_duration_since(*t) > self.window)
        {
            self.samples.pop_front();
        }
        self.samples.front().map_or(value, |(_, v)| *v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_peaks_expire() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut p = RecentPeak::new(s(60));
        assert_eq!(p.push(t0, 5000), 5000);
        assert_eq!(p.push(t0 + s(10), 3000), 5000);
        assert_eq!(p.push(t0 + s(59), 3200), 5000);
        // The 5000 burst is now more than a minute old.
        assert_eq!(p.push(t0 + s(61), 3100), 3200);
        assert_eq!(p.push(t0 + s(200), 2000), 2000);
        // Bounded: never more samples than distinct descending values.
        for i in 0..10_000 {
            p.push(t0 + s(300) + Duration::from_millis(i), 1000);
        }
        assert!(p.samples.len() <= 2);
    }
}
