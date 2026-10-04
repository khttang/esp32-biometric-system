//! Verification metrics from similarity scores.
//!
//! A face matcher is judged on two kinds of comparison: *genuine* pairs (two images of the same
//! person) and *impostor* pairs (images of two different people). For a given threshold,
//!
//! - the false accept rate (FAR) is the share of impostor pairs that score at or above it, and
//! - the false reject rate (FRR) is the share of genuine pairs that score below it,
//!
//! matching [`crate::matching::best_match`], which accepts a score equal to the threshold.

use crate::matching::cosine_similarity;

/// Genuine and impostor similarity scores, each sorted ascending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scores {
    genuine: Vec<f32>,
    impostor: Vec<f32>,
}

impl Scores {
    /// Non-finite scores are dropped: they cannot be compared with a threshold.
    pub fn new(mut genuine: Vec<f32>, mut impostor: Vec<f32>) -> Self {
        for scores in [&mut genuine, &mut impostor] {
            scores.retain(|s| s.is_finite());
            scores.sort_unstable_by(f32::total_cmp);
        }
        Self { genuine, impostor }
    }

    /// Scores of every pair of `embeddings`; a pair is genuine if its `labels` are equal.
    ///
    /// Embeddings must be L2-normalised and of one length. `labels` and `embeddings` are
    /// parallel; extra entries of the longer one are ignored.
    pub fn from_pairs<E: AsRef<[f32]>>(embeddings: &[E], labels: &[u32]) -> Self {
        let n = embeddings.len().min(labels.len());
        let (mut genuine, mut impostor) = (Vec::new(), Vec::new());
        for i in 0..n {
            for j in i + 1..n {
                let score = cosine_similarity(embeddings[i].as_ref(), embeddings[j].as_ref());
                if labels[i] == labels[j] {
                    genuine.push(score);
                } else {
                    impostor.push(score);
                }
            }
        }
        Self::new(genuine, impostor)
    }

    pub fn genuine(&self) -> &[f32] {
        &self.genuine
    }

    pub fn impostor(&self) -> &[f32] {
        &self.impostor
    }

    /// Share of impostor pairs accepted at `threshold`; `None` without impostor pairs.
    pub fn far(&self, threshold: f32) -> Option<f64> {
        let accepted = self.impostor.len() - self.impostor.partition_point(|&s| s < threshold);
        ratio(accepted, self.impostor.len())
    }

    /// Share of genuine pairs rejected at `threshold`; `None` without genuine pairs.
    pub fn frr(&self, threshold: f32) -> Option<f64> {
        let rejected = self.genuine.partition_point(|&s| s < threshold);
        ratio(rejected, self.genuine.len())
    }

    /// The lowest threshold whose FAR does not exceed `target`; `None` without impostor pairs.
    ///
    /// With `n` impostor pairs the smallest measurable non-zero FAR is `1/n`, so a `target`
    /// below that yields the threshold just above the highest impostor score.
    pub fn threshold_for_far(&self, target: f64) -> Option<f32> {
        let n = self.impostor.len();
        if n == 0 {
            return None;
        }
        let allowed = (target.max(0.0) * n as f64).floor() as usize;
        Some(match n.checked_sub(allowed + 1) {
            // Reject the (allowed + 1)-th highest impostor score and everything equal to it.
            Some(index) => self.impostor[index].next_up(),
            None => self.impostor[0],
        })
    }

    /// Equal error rate: the threshold where FAR and FRR are closest, and their mean there.
    /// `None` unless there are both genuine and impostor pairs.
    pub fn equal_error(&self) -> Option<(f32, f64)> {
        if self.genuine.is_empty() || self.impostor.is_empty() {
            return None;
        }
        // FAR falls and FRR rises with the threshold, so their difference changes sign once.
        let mut candidates: Vec<f32> = self.genuine.iter().chain(&self.impostor).copied().collect();
        candidates.sort_unstable_by(f32::total_cmp);
        candidates.push(candidates[candidates.len() - 1].next_up());
        let gap = |t: f32| {
            self.far(t)
                .zip(self.frr(t))
                .map(|(far, frr)| (far - frr, far, frr))
        };
        let crossing = candidates.partition_point(|&t| gap(t).is_some_and(|(d, ..)| d > 0.0));
        [crossing.checked_sub(1), Some(crossing)]
            .into_iter()
            .flatten()
            .filter_map(|i| candidates.get(i).copied())
            .filter_map(|t| gap(t).map(|(d, far, frr)| (t, d.abs(), (far + frr) / 2.0)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(t, _, rate)| (t, rate))
    }
}

fn ratio(count: usize, total: usize) -> Option<f64> {
    (total > 0).then(|| count as f64 / total as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scores() -> Scores {
        // 5 genuine, 10 impostor; they overlap between 0.4 and 0.6.
        Scores::new(
            vec![0.9, 0.8, 0.7, 0.55, 0.45],
            vec![0.6, 0.5, 0.4, 0.3, 0.2, 0.1, 0.0, -0.1, -0.2, 0.25],
        )
    }

    #[test]
    fn far_counts_impostors_at_or_above_the_threshold() {
        let s = scores();
        assert_eq!(s.far(0.5), Some(0.2)); // 0.6 and 0.5
        assert_eq!(s.far(0.61), Some(0.0));
        assert_eq!(s.far(-1.0), Some(1.0));
    }

    #[test]
    fn frr_counts_genuine_below_the_threshold() {
        let s = scores();
        assert_eq!(s.frr(0.5), Some(0.2)); // 0.45
        assert_eq!(s.frr(0.45), Some(0.0)); // the threshold itself is accepted
        assert_eq!(s.frr(1.0), Some(1.0));
    }

    #[test]
    fn rates_are_undefined_without_pairs() {
        let s = Scores::new(vec![], vec![]);
        assert_eq!(s.far(0.5), None);
        assert_eq!(s.frr(0.5), None);
        assert_eq!(s.threshold_for_far(0.01), None);
        assert_eq!(s.equal_error(), None);
        assert_eq!(Scores::new(vec![0.9], vec![]).equal_error(), None);
    }

    #[test]
    fn threshold_for_far_is_the_lowest_that_meets_the_target() {
        let s = scores();
        // 10% of 10 pairs: one impostor (0.6) may pass, 0.5 must not.
        let t = s.threshold_for_far(0.1).unwrap();
        assert!(t > 0.5 && t <= 0.5f32.next_up());
        assert_eq!(s.far(t), Some(0.1));
        // A target below 1/n allows none.
        let t = s.threshold_for_far(0.01).unwrap();
        assert_eq!(s.far(t), Some(0.0));
        assert!(t <= 0.6f32.next_up());
        // A target of 100% is met by any threshold.
        assert_eq!(s.far(s.threshold_for_far(1.0).unwrap()), Some(1.0));
    }

    #[test]
    fn threshold_for_far_rejects_all_tied_scores() {
        let s = Scores::new(vec![0.9], vec![0.5, 0.5, 0.5, 0.1]);
        // One of four may pass, but the three ties cannot be split.
        let t = s.threshold_for_far(0.25).unwrap();
        assert_eq!(s.far(t), Some(0.0));
    }

    #[test]
    fn equal_error_of_separable_scores_is_zero() {
        let s = Scores::new(vec![0.8, 0.9], vec![0.1, 0.2]);
        let (threshold, rate) = s.equal_error().unwrap();
        assert_eq!(rate, 0.0);
        assert!(threshold > 0.2 && threshold <= 0.8);
    }

    #[test]
    fn equal_error_of_overlapping_scores() {
        let (threshold, rate) = scores().equal_error().unwrap();
        // At 0.5: FAR 0.2, FRR 0.2.
        assert!((rate - 0.2).abs() < 1e-9, "{rate}");
        assert!(threshold > 0.45 && threshold <= 0.5, "{threshold}");
    }

    #[test]
    fn equal_error_of_an_inverted_matcher_is_total() {
        // The impostor outscores the genuine pair: at a threshold between them (up to and
        // including the impostor's score) the genuine pair is rejected and the impostor accepted.
        let s = Scores::new(vec![0.1], vec![0.9]);
        let (threshold, rate) = s.equal_error().unwrap();
        assert_eq!(rate, 1.0);
        assert!(threshold > 0.1 && threshold <= 0.9, "{threshold}");
    }

    #[test]
    fn non_finite_scores_are_dropped() {
        let s = Scores::new(vec![f32::NAN, 0.9], vec![f32::INFINITY, 0.1]);
        assert_eq!(s.genuine(), &[0.9]);
        assert_eq!(s.impostor(), &[0.1]);
    }

    #[test]
    fn pairs_are_split_by_label() {
        let embeddings = [[1.0, 0.0], [1.0, 0.0], [0.0, 1.0], [0.6, 0.8]];
        let s = Scores::from_pairs(&embeddings, &[7, 7, 8, 8]);
        // Genuine: (0,1) = 1.0 and (2,3) = 0.8. Impostor: the other four pairs.
        assert_eq!(s.genuine(), &[0.8, 1.0]);
        assert_eq!(s.impostor(), &[0.0, 0.0, 0.6, 0.6]);
    }
}
