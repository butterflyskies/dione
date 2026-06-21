//! Change-point detection for conversation branches.
//!
//! Two implementations behind the [`ChangePointDetector`] trait:
//!
//! - [`BayesianDetector`] (ariadne): stateful, maintains a full run-length
//!   distribution, three-state verdict (ChangePoint/Continuation/Uncertain).
//!   Uses Adams & MacKay (2007) with an exponential generative model.
//!
//! - [`LightweightDetector`] (Lain): stateless per-call, reconstructs a
//!   point-mass distribution from the current run-length each call.
//!   Two-state verdict (ChangePoint/Continuation).

use super::Branch;

// ── Newtypes ────────────────────────────────────────────────────────────────

/// Probability value clamped to `[0.0, 1.0]`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Probability(f64);

impl Probability {
    /// Create a probability, clamping to `[0.0, 1.0]`.
    pub fn new(p: f64) -> Self {
        Self(p.clamp(0.0, 1.0))
    }

    /// The raw `f64` value.
    pub fn value(self) -> f64 {
        self.0
    }

    /// Certainty: `P = 1.0`.
    pub const ONE: Self = Self(1.0);

    /// Impossibility: `P = 0.0`.
    pub const ZERO: Self = Self(0.0);
}

impl std::fmt::Display for Probability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:.4}", self.0)
    }
}

/// A positive rate parameter (messages per second or expected gap).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct PositiveRate(f64);

impl PositiveRate {
    /// Create a positive rate, clamping to `[f64::MIN_POSITIVE, f64::MAX]`.
    pub fn new(r: f64) -> Self {
        Self(r.max(f64::MIN_POSITIVE))
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the CPD layer, shared by both detector implementations.
#[derive(Debug, Clone)]
pub struct CpdConfig {
    /// Expected run length (lambda) for the hazard function.
    /// Default: 20.0.
    pub expected_run_length: f64,
    /// Threshold for declaring a changepoint.
    /// - Bayesian: ratio `P(r=0) / max(P(r>0))` must exceed this.
    ///   - Lightweight: posterior `P(r=0)` must exceed this.
    ///
    /// Default: 0.5.
    pub changepoint_threshold: f64,
    /// Minimum probability below which run-lengths are pruned.
    /// Default: 1e-4.
    pub pruning_threshold: f64,
    /// Maximum run-length vector size (hard cap). Default: 100.
    pub max_run_lengths: usize,
    /// Minimum confidence from the rate estimator before the Bayesian CPD
    /// produces meaningful results. Below this, returns `Uncertain`.
    /// Default: 0.3. (Not used by the lightweight detector.)
    pub min_confidence: f32,
}

impl Default for CpdConfig {
    fn default() -> Self {
        Self {
            expected_run_length: 20.0,
            changepoint_threshold: 0.5,
            pruning_threshold: 1e-4,
            max_run_lengths: 100,
            min_confidence: 0.3,
        }
    }
}

// ── Verdict ─────────────────────────────────────────────────────────────────

/// Three-state outcome of a change-point detection step.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// A changepoint was detected — the topic/conversation has shifted.
    ChangePoint {
        /// Posterior probability of changepoint.
        probability: Probability,
    },
    /// The current run continues — no evidence of a topic shift.
    Continuation {
        /// Most probable run length.
        run_length: u32,
    },
    /// Not enough data to make a meaningful determination.
    /// (Only produced by BayesianDetector.)
    Uncertain {
        /// Rate estimator confidence at the time of the call.
        confidence: f32,
    },
}

impl Verdict {
    /// Whether the verdict indicates a change-point.
    pub fn is_changepoint(&self) -> bool {
        matches!(self, Self::ChangePoint { .. })
    }
}

// ── Run-length distribution ────────────────────────────────────────────────

/// Per-branch run-length probability distribution.
///
/// `distribution[i]` = `P(run_length = i)`. Index 0 is the changepoint
/// probability; higher indices represent longer continuation runs.
#[derive(Debug, Clone)]
pub struct RunLengthDistribution {
    /// Probability mass for each run length. `dist[0]` = P(changepoint).
    dist: Vec<f64>,
}

impl RunLengthDistribution {
    /// Initialize with a single run-length of 0 at probability 1.0.
    pub fn new() -> Self {
        Self { dist: vec![1.0] }
    }

    /// Number of tracked run-lengths.
    pub fn len(&self) -> usize {
        self.dist.len()
    }

    /// Whether the distribution is empty.
    pub fn is_empty(&self) -> bool {
        self.dist.is_empty()
    }

    /// The changepoint probability: `P(r = 0)`.
    pub fn changepoint_probability(&self) -> Probability {
        Probability::new(self.dist.first().copied().unwrap_or(0.0))
    }

    /// The maximum continuation probability: `max(P(r > 0))`.
    pub fn max_continuation_probability(&self) -> Probability {
        Probability::new(self.dist.iter().skip(1).copied().fold(0.0_f64, f64::max))
    }

    /// The most probable run length.
    pub fn mode(&self) -> u32 {
        self.dist
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i as u32)
            .unwrap_or(0)
    }

    /// Access the raw distribution slice (for testing/debugging).
    pub fn as_slice(&self) -> &[f64] {
        &self.dist
    }
}

impl Default for RunLengthDistribution {
    fn default() -> Self {
        Self::new()
    }
}

// ── ChangePointDetector trait ──────────────────────────────────────────────

/// Trait for change-point detection strategies.
///
/// Both the stateful Bayesian detector and the stateless lightweight
/// detector implement this trait with the same signature.
pub trait ChangePointDetector {
    /// Run one step of change-point detection.
    ///
    /// # Arguments
    /// - `dist`: the current run-length distribution (mutated in place).
    /// - `gap`: the observed inter-message gap in seconds.
    /// - `rate`: the current rate estimate from the branch's EWMA.
    /// - `config`: CPD configuration.
    ///
    /// # Returns
    /// A [`Verdict`] indicating whether a change-point was detected.
    fn detect(
        &self,
        dist: &mut RunLengthDistribution,
        gap: f64,
        rate: f64,
        config: &CpdConfig,
    ) -> Verdict;

    /// Whether this detector should be gated by `min_confidence`.
    ///
    /// The Bayesian detector produces `Uncertain` when confidence is low;
    /// the lightweight detector only produces `ChangePoint`/`Continuation`
    /// and should not be gated.
    fn requires_confidence_gate(&self) -> bool {
        false
    }
}

// ── Exponential PDF ────────────────────────────────────────────────────────

/// Exponential PDF: `f(x; rate) = rate * exp(-rate * x)` for `x >= 0`.
#[inline]
fn exp_pdf(gap: f64, rate: f64) -> f64 {
    if gap < 0.0 || rate <= 0.0 {
        return 0.0;
    }
    rate * (-rate * gap).exp()
}

// ── BayesianDetector ───────────────────────────────────────────────────────

/// Stateful Bayesian online change-point detector (Adams & MacKay 2007).
///
/// Maintains the full run-length distribution across calls. Produces
/// three-state verdicts: ChangePoint, Continuation, or Uncertain (when
/// rate confidence is too low).
#[derive(Debug, Clone, Default)]
pub struct BayesianDetector;

impl BayesianDetector {
    pub fn new() -> Self {
        Self
    }
}

impl ChangePointDetector for BayesianDetector {
    fn requires_confidence_gate(&self) -> bool {
        true
    }

    fn detect(
        &self,
        dist: &mut RunLengthDistribution,
        gap: f64,
        rate: f64,
        config: &CpdConfig,
    ) -> Verdict {
        let mean_gap = rate.max(f64::MIN_POSITIVE);
        let lambda = 1.0 / mean_gap;

        // Predictive probability of the observed gap.
        let predictive = exp_pdf(gap, lambda);

        // Hazard rate from config.
        let h = 1.0 / config.expected_run_length.max(1.0);
        let h_complement = 1.0 - h;

        // ── Run-length update ──────────────────────────────────────────
        let old_dist = &dist.dist;
        let n = old_dist.len();

        let mut changepoint_mass = 0.0_f64;
        let mut new_dist = Vec::with_capacity((n + 1).min(config.max_run_lengths + 1));
        new_dist.push(0.0); // Placeholder for changepoint.

        for &p_r in old_dist.iter() {
            let joint = p_r * predictive;
            let growth = joint * h_complement;
            changepoint_mass += joint * h;
            new_dist.push(growth);
        }

        new_dist[0] = changepoint_mass;

        // ── Normalize ──────────────────────────────────────────────────
        let total: f64 = new_dist.iter().sum();
        if total > 0.0 {
            let inv_total = 1.0 / total;
            for p in new_dist.iter_mut() {
                *p *= inv_total;
            }
        } else {
            new_dist.clear();
            new_dist.push(1.0);
        }

        // ── Prune ──────────────────────────────────────────────────────
        while new_dist.len() > 1
            && new_dist
                .last()
                .is_some_and(|&p| p < config.pruning_threshold)
        {
            new_dist.pop();
        }

        if new_dist.len() > config.max_run_lengths {
            new_dist.truncate(config.max_run_lengths);
            let total: f64 = new_dist.iter().sum();
            if total > 0.0 {
                let inv_total = 1.0 / total;
                for p in new_dist.iter_mut() {
                    *p *= inv_total;
                }
            }
        }

        dist.dist = new_dist;

        // ── Verdict ────────────────────────────────────────────────────
        let cp_prob = dist.changepoint_probability();
        let max_cont = dist.max_continuation_probability();

        let ratio = if max_cont.value() > 0.0 {
            cp_prob.value() / max_cont.value()
        } else {
            f64::INFINITY
        };

        if ratio > config.changepoint_threshold {
            Verdict::ChangePoint {
                probability: cp_prob,
            }
        } else {
            Verdict::Continuation {
                run_length: dist.mode(),
            }
        }
    }
}

// ── LightweightDetector ────────────────────────────────────────────────────

/// Stateless lightweight change-point detector.
///
/// Reconstructs a point-mass distribution from the current run-length
/// each call rather than maintaining the full distribution. Lighter weight,
/// but loses inter-call distributional information.
///
/// Uses two different rate parameters:
/// - The branch's own rate estimate for continuation probability.
/// - A channel-wide prior rate for the changepoint hypothesis.
#[derive(Debug, Clone)]
pub struct LightweightDetector {
    /// Channel-wide prior rate estimate (mean gap in seconds).
    /// Used as the changepoint prior — "what does a new topic look like?"
    pub channel_prior_rate: f64,
}

impl LightweightDetector {
    /// Create a new lightweight detector.
    ///
    /// # Arguments
    /// - `channel_prior_rate`: channel baseline mean gap (seconds). Used as
    ///   the prior for the changepoint hypothesis.
    pub fn new(channel_prior_rate: f64) -> Self {
        Self { channel_prior_rate }
    }
}

impl ChangePointDetector for LightweightDetector {
    fn detect(
        &self,
        dist: &mut RunLengthDistribution,
        gap: f64,
        rate: f64,
        config: &CpdConfig,
    ) -> Verdict {
        // Reconstruct current run-length from distribution mode.
        let current_run_length = dist.mode();
        let dist_len = (current_run_length as usize + 1).min(config.max_run_lengths);

        // Build point-mass prior at the current run-length.
        let mut prior = vec![0.0f64; dist_len + 1];
        if (current_run_length as usize) < prior.len() {
            prior[current_run_length as usize] = 1.0;
        }

        let hazard = 1.0 / config.expected_run_length.max(1.0);

        // Predictive probabilities.
        let pred_growth = exp_pdf(gap, 1.0 / rate.max(f64::MIN_POSITIVE));
        let pred_cp = exp_pdf(gap, 1.0 / self.channel_prior_rate.max(f64::MIN_POSITIVE));

        // Run-length update.
        let mut new_dist = vec![0.0f64; dist_len + 2];

        let mut cp_mass = 0.0;
        for p_r in &prior {
            cp_mass += pred_cp * hazard * p_r;
        }
        new_dist[0] = cp_mass;

        for (r, p_r) in prior.iter().enumerate() {
            let grown = r + 1;
            if grown < new_dist.len() {
                new_dist[grown] += pred_growth * (1.0 - hazard) * p_r;
            }
        }

        // Normalize.
        let total: f64 = new_dist.iter().sum();
        if total > 0.0 {
            for p in &mut new_dist {
                *p /= total;
            }
        }

        // Prune.
        for p in &mut new_dist {
            if *p < config.pruning_threshold {
                *p = 0.0;
            }
        }

        let total: f64 = new_dist.iter().sum();
        if total > 0.0 {
            for p in &mut new_dist {
                *p /= total;
            }
        }

        let posterior_cp = new_dist[0];

        // MAP run-length.
        let map_run_length = new_dist
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i as u32)
            .unwrap_or(0);

        // Store back into dist for the next call (partial statefulness).
        dist.dist = new_dist;

        if posterior_cp > config.changepoint_threshold {
            Verdict::ChangePoint {
                probability: Probability::new(posterior_cp),
            }
        } else {
            Verdict::Continuation {
                run_length: map_run_length,
            }
        }
    }
}

// ── Convenience: detect with branch mutation ───────────────────────────────

/// Run change-point detection on a branch, updating its run-length counter.
///
/// Wraps the [`ChangePointDetector`] trait call with branch state mutation:
/// - On `ChangePoint`: resets the branch's run length to 0.
/// - On `Continuation`: increments the branch's run length.
/// - On `Uncertain`: increments the branch's run length (no evidence yet).
pub fn detect_and_update(
    detector: &dyn ChangePointDetector,
    branch: &mut Branch,
    dist: &mut RunLengthDistribution,
    gap: f64,
    config: &CpdConfig,
) -> Verdict {
    // Gate on confidence — only applies to detectors that require it
    // (BayesianDetector). LightweightDetector produces only ChangePoint/
    // Continuation and should not be gated.
    if detector.requires_confidence_gate() {
        let confidence = branch.rate_confidence();
        if confidence < config.min_confidence {
            branch.increment_run_length();
            return Verdict::Uncertain { confidence };
        }
    }

    let rate = branch.rate_estimate();
    let verdict = detector.detect(dist, gap, rate, config);

    match &verdict {
        Verdict::ChangePoint { .. } => branch.reset_run_length(),
        Verdict::Continuation { .. } | Verdict::Uncertain { .. } => {
            branch.increment_run_length();
        }
    }

    verdict
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_tracking::ewma::EwmaEstimator;
    use crate::branch_tracking::{Branch, BranchId, RateEstimator};
    use std::time::Instant;

    /// Helper: create a Branch with a known rate estimate.
    fn make_branch(rate_value: f64, observations: u32) -> Branch {
        let mut estimator = EwmaEstimator::new(0.2, rate_value);
        if observations > 0 {
            estimator.observe(rate_value);
            for _ in 1..observations {
                estimator.observe(rate_value);
            }
        }
        Branch::new(
            BranchId::new_for_test(0),
            Box::new(estimator),
            Instant::now(),
        )
    }

    // ── Probability newtype ────────────────────────────────────────────

    #[test]
    fn probability_clamps() {
        assert_eq!(Probability::new(-0.5).value(), 0.0);
        assert_eq!(Probability::new(1.5).value(), 1.0);
        assert_eq!(Probability::new(0.5).value(), 0.5);
    }

    #[test]
    fn probability_display() {
        let p = Probability::new(0.12345);
        assert_eq!(format!("{p}"), "0.1235");
    }

    #[test]
    fn probability_constants() {
        assert_eq!(Probability::ONE.value(), 1.0);
        assert_eq!(Probability::ZERO.value(), 0.0);
    }

    // ── PositiveRate newtype ───────────────────────────────────────────

    #[test]
    fn positive_rate_clamps_zero() {
        let r = PositiveRate::new(0.0);
        assert!(r.value() > 0.0);
    }

    #[test]
    fn positive_rate_preserves_positive() {
        let r = PositiveRate::new(42.0);
        assert_eq!(r.value(), 42.0);
    }

    // ── RunLengthDistribution ─────────────────────────────────────────

    #[test]
    fn run_length_distribution_initial() {
        let rld = RunLengthDistribution::new();
        assert_eq!(rld.len(), 1);
        assert_eq!(rld.changepoint_probability().value(), 1.0);
        assert_eq!(rld.max_continuation_probability().value(), 0.0);
        assert_eq!(rld.mode(), 0);
    }

    #[test]
    fn run_length_distribution_default() {
        let rld = RunLengthDistribution::default();
        assert_eq!(rld.len(), 1);
    }

    #[test]
    fn run_length_distribution_mode() {
        let rld = RunLengthDistribution {
            dist: vec![0.1, 0.3, 0.5, 0.1],
        };
        assert_eq!(rld.mode(), 2);
    }

    // ── exp_pdf ───────────────────────────────────────────────────────

    #[test]
    fn exp_pdf_at_zero() {
        let p = exp_pdf(0.0, 2.0);
        assert!((p - 2.0).abs() < 1e-10);
    }

    #[test]
    fn exp_pdf_positive_gap() {
        let p = exp_pdf(1.0, 1.0);
        assert!((p - (-1.0_f64).exp()).abs() < 1e-10);
    }

    #[test]
    fn exp_pdf_negative_gap_is_zero() {
        assert_eq!(exp_pdf(-1.0, 1.0), 0.0);
    }

    #[test]
    fn exp_pdf_zero_rate_is_zero() {
        assert_eq!(exp_pdf(1.0, 0.0), 0.0);
    }

    // ── BayesianDetector ─────────────────────────────────────────────

    #[test]
    fn bayesian_continuation_on_expected_gap() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        // Steady stream at rate=10s.
        for _ in 0..5 {
            let v = detector.detect(&mut dist, 10.0, 10.0, &config);
            assert!(
                !v.is_changepoint(),
                "expected gap should continue, got {v:?}"
            );
        }
    }

    #[test]
    fn bayesian_detects_changepoint_on_large_shift() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig {
            expected_run_length: 5.0,
            changepoint_threshold: 1.0,
            ..Default::default()
        };

        // Build up a run.
        for _ in 0..5 {
            detector.detect(&mut dist, 5.0, 5.0, &config);
        }

        // Anomalous gaps.
        let mut detected_cp = false;
        for _ in 0..10 {
            let v = detector.detect(&mut dist, 30.0, 5.0, &config);
            if v.is_changepoint() {
                detected_cp = true;
                break;
            }
        }
        assert!(
            detected_cp,
            "should detect changepoint after sustained anomaly"
        );
    }

    #[test]
    fn bayesian_distribution_sums_to_one() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        for _ in 0..20 {
            detector.detect(&mut dist, 10.0, 10.0, &config);
        }

        let sum: f64 = dist.as_slice().iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "distribution should sum to ~1.0, got {sum}"
        );
    }

    #[test]
    fn bayesian_pruning_keeps_distribution_compact() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig {
            pruning_threshold: 1e-4,
            max_run_lengths: 100,
            ..Default::default()
        };

        for _ in 0..200 {
            detector.detect(&mut dist, 10.0, 10.0, &config);
        }
        assert!(dist.len() <= config.max_run_lengths);
    }

    // ── LightweightDetector ──────────────────────────────────────────

    #[test]
    fn lightweight_continuation_at_expected_rate() {
        let detector = LightweightDetector::new(60.0);
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let v = detector.detect(&mut dist, 10.0, 10.0, &config);
        assert!(
            !v.is_changepoint(),
            "gap matching rate should continue, got {v:?}"
        );
    }

    #[test]
    fn lightweight_changepoint_at_extreme_gap() {
        let detector = LightweightDetector::new(60.0);
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig {
            changepoint_threshold: 0.3,
            ..Default::default()
        };

        // Build up some run.
        for _ in 0..5 {
            detector.detect(&mut dist, 10.0, 10.0, &config);
        }

        let v = detector.detect(&mut dist, 500.0, 10.0, &config);
        assert!(
            v.is_changepoint(),
            "gap 50x rate should be a changepoint, got {v:?}"
        );
    }

    #[test]
    fn lightweight_cold_start() {
        let detector = LightweightDetector::new(60.0);
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let v = detector.detect(&mut dist, 15.0, 60.0, &config);
        // Should produce a valid verdict without panicking.
        match v {
            Verdict::ChangePoint { probability } => {
                assert!(probability.value() >= 0.0 && probability.value() <= 1.0);
            }
            Verdict::Continuation { .. } => {}
            Verdict::Uncertain { .. } => {}
        }
    }

    // ── detect_and_update ────────────────────────────────────────────

    #[test]
    fn detect_and_update_uncertain_when_low_confidence() {
        let detector = BayesianDetector::new();
        let mut branch = make_branch(10.0, 0); // 0 observations -> confidence 0.0
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let v = detect_and_update(&detector, &mut branch, &mut dist, 5.0, &config);
        match v {
            Verdict::Uncertain { confidence } => {
                assert_eq!(confidence, 0.0);
            }
            _ => panic!("expected Uncertain, got {v:?}"),
        }
        assert_eq!(branch.run_length(), 1);
    }

    #[test]
    fn detect_and_update_increments_on_continuation() {
        let detector = BayesianDetector::new();
        let mut branch = make_branch(10.0, 10);
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        for i in 1..=5 {
            let v = detect_and_update(&detector, &mut branch, &mut dist, 10.0, &config);
            assert!(!v.is_changepoint());
            assert_eq!(branch.run_length(), i);
        }
    }

    #[test]
    fn detect_and_update_resets_on_changepoint() {
        let detector = BayesianDetector::new();
        let mut branch = make_branch(5.0, 10);
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig {
            expected_run_length: 5.0,
            changepoint_threshold: 1.0,
            ..Default::default()
        };

        // Build up run.
        for _ in 0..5 {
            detect_and_update(&detector, &mut branch, &mut dist, 5.0, &config);
        }

        // Force changepoint.
        let mut detected = false;
        for _ in 0..10 {
            let v = detect_and_update(&detector, &mut branch, &mut dist, 30.0, &config);
            if v.is_changepoint() {
                detected = true;
                assert_eq!(branch.run_length(), 0);
                break;
            }
        }
        assert!(detected, "should detect changepoint");
    }

    // ── Both detectors compared ─────────────────────────────────────

    #[test]
    fn both_detectors_agree_on_steady_state() {
        let bayesian = BayesianDetector::new();
        let lightweight = LightweightDetector::new(60.0);
        let config = CpdConfig::default();

        let mut dist_b = RunLengthDistribution::new();
        let mut dist_l = RunLengthDistribution::new();

        // Both should agree that steady-state gaps are continuations.
        for _ in 0..10 {
            let vb = bayesian.detect(&mut dist_b, 10.0, 10.0, &config);
            let vl = lightweight.detect(&mut dist_l, 10.0, 10.0, &config);
            assert!(!vb.is_changepoint());
            assert!(!vl.is_changepoint());
        }
    }

    // ── CpdConfig defaults ──────────────────────────────────────────

    #[test]
    fn cpd_config_defaults() {
        let config = CpdConfig::default();
        assert_eq!(config.expected_run_length, 20.0);
        assert_eq!(config.changepoint_threshold, 0.5);
        assert_eq!(config.pruning_threshold, 1e-4);
        assert_eq!(config.max_run_lengths, 100);
        assert_eq!(config.min_confidence, 0.3);
    }

    // ── Edge cases ──────────────────────────────────────────────────

    #[test]
    fn zero_gap_does_not_panic() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let _v = detector.detect(&mut dist, 0.0, 10.0, &config);
        assert!(!dist.is_empty());
    }

    #[test]
    fn very_large_gap_does_not_panic() {
        let detector = BayesianDetector::new();
        let mut dist = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let _v = detector.detect(&mut dist, 1e10, 1.0, &config);
        assert!(!dist.is_empty());
    }
}
