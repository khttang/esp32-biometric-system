//! Allocation-free latency statistics for on-device performance logging.

use core::time::Duration;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LatencyStats {
    count: u32,
    total_us: u64,
    max_us: u64,
}

impl LatencyStats {
    pub fn record(&mut self, sample: Duration) {
        let us = u64::try_from(sample.as_micros()).unwrap_or(u64::MAX);
        self.count = self.count.saturating_add(1);
        self.total_us = self.total_us.saturating_add(us);
        self.max_us = self.max_us.max(us);
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Mean sample, or `None` if nothing was recorded.
    pub fn mean(&self) -> Option<Duration> {
        (self.count > 0).then(|| Duration::from_micros(self.total_us / u64::from(self.count)))
    }

    pub fn max(&self) -> Option<Duration> {
        (self.count > 0).then(|| Duration::from_micros(self.max_us))
    }

    /// Returns the current statistics and starts a new window.
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stats_have_no_mean_or_max() {
        let s = LatencyStats::default();
        assert_eq!((s.count(), s.mean(), s.max()), (0, None, None));
    }

    #[test]
    fn mean_and_max_over_samples() {
        let mut s = LatencyStats::default();
        for ms in [10, 20, 60] {
            s.record(Duration::from_millis(ms));
        }
        assert_eq!(s.count(), 3);
        assert_eq!(s.mean(), Some(Duration::from_millis(30)));
        assert_eq!(s.max(), Some(Duration::from_millis(60)));
    }

    #[test]
    fn take_returns_window_and_resets() {
        let mut s = LatencyStats::default();
        s.record(Duration::from_millis(5));
        let window = s.take();
        assert_eq!(window.count(), 1);
        assert_eq!(s, LatencyStats::default());
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        let mut s = LatencyStats::default();
        s.record(Duration::MAX);
        s.record(Duration::MAX);
        assert_eq!(s.max(), Some(Duration::from_micros(u64::MAX)));
        assert_eq!(s.count(), 2);
    }
}
