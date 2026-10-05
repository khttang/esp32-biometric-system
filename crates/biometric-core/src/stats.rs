//! Allocation-free statistics for on-device performance logging.

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
}

/// Range and mean of similarity scores over a logging window, used to judge the match
/// threshold against what the device actually sees.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct ScoreStats {
    count: u32,
    total: f32,
    min: f32,
    max: f32,
}

impl ScoreStats {
    /// Records one score; non-finite scores are ignored.
    pub fn record(&mut self, score: f32) {
        if !score.is_finite() {
            return;
        }
        if self.count == 0 {
            (self.min, self.max) = (score, score);
        } else {
            (self.min, self.max) = (self.min.min(score), self.max.max(score));
        }
        self.count = self.count.saturating_add(1);
        self.total += score;
    }

    pub fn count(&self) -> u32 {
        self.count
    }

    /// Mean score, or `None` if nothing was recorded.
    pub fn mean(&self) -> Option<f32> {
        (self.count > 0).then(|| self.total / self.count as f32)
    }

    pub fn min(&self) -> Option<f32> {
        (self.count > 0).then_some(self.min)
    }

    pub fn max(&self) -> Option<f32> {
        (self.count > 0).then_some(self.max)
    }
}

/// Loudness of 16-bit PCM audio over a logging window: shows whether a microphone is alive
/// (a dead input reads as constant zeros) and whether it clips.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioLevel {
    samples: u64,
    sum_of_squares: u64,
    peak: u16,
}

impl AudioLevel {
    pub fn record(&mut self, pcm: &[i16]) {
        for &sample in pcm {
            let magnitude = sample.unsigned_abs();
            self.peak = self.peak.max(magnitude);
            self.sum_of_squares += u64::from(magnitude) * u64::from(magnitude);
        }
        self.samples += pcm.len() as u64;
    }

    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// Largest absolute sample value; 32768 means the input clipped.
    pub fn peak(&self) -> u16 {
        self.peak
    }

    /// Root mean square of the samples, or `None` if nothing was recorded.
    pub fn rms(&self) -> Option<f32> {
        (self.samples > 0).then(|| (self.sum_of_squares as f64 / self.samples as f64).sqrt() as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_score_stats_have_no_values() {
        let s = ScoreStats::default();
        assert_eq!(
            (s.count(), s.mean(), s.min(), s.max()),
            (0, None, None, None)
        );
    }

    #[test]
    fn score_range_and_mean_include_negative_scores() {
        let mut s = ScoreStats::default();
        for score in [0.5, -0.25, 0.75] {
            s.record(score);
        }
        assert_eq!(s.count(), 3);
        assert_eq!(s.min(), Some(-0.25));
        assert_eq!(s.max(), Some(0.75));
        assert!((s.mean().unwrap() - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn score_stats_ignore_non_finite_values() {
        let mut s = ScoreStats::default();
        s.record(f32::NAN);
        s.record(f32::INFINITY);
        assert_eq!(s.count(), 0);
        s.record(0.4);
        assert_eq!((s.min(), s.max()), (Some(0.4), Some(0.4)));
    }

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
    fn saturates_instead_of_overflowing() {
        let mut s = LatencyStats::default();
        s.record(Duration::MAX);
        s.record(Duration::MAX);
        assert_eq!(s.max(), Some(Duration::from_micros(u64::MAX)));
        assert_eq!(s.count(), 2);
    }

    #[test]
    fn audio_level_of_silence_and_of_nothing() {
        let mut level = AudioLevel::default();
        assert_eq!(level.rms(), None);
        level.record(&[0; 64]);
        assert_eq!(
            (level.samples(), level.peak(), level.rms()),
            (64, 0, Some(0.0))
        );
    }

    #[test]
    fn audio_level_tracks_peak_and_rms_across_frames() {
        let mut level = AudioLevel::default();
        level.record(&[3, -4]);
        level.record(&[0, 0]);
        assert_eq!(level.peak(), 4);
        // sqrt((9 + 16) / 4)
        assert_eq!(level.rms(), Some(2.5));
        assert_eq!(level.samples(), 4);
    }

    #[test]
    fn audio_level_handles_the_most_negative_sample() {
        let mut level = AudioLevel::default();
        level.record(&[i16::MIN, i16::MAX]);
        assert_eq!(level.peak(), 32768);
        assert!(level.rms().unwrap() > 32767.0);
    }
}
