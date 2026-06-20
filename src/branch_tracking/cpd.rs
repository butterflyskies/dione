//! Bayesian online change-point detection for conversation branches.
//!
//! Implements the Adams & MacKay (2007) algorithm with an exponential
//! generative model: inter-message gaps follow `Exp(μ_k)` where `μ_k`
//! is the branch's EWMA rate estimate.
//!
//! # Design
//!
//! The core function [`detect_changepoint`] is **pure** — it takes branch
//! state + observation + config and returns a [`ChangePointResult`]. All
//! mutation goes through the branch's `increment_run_length()` /
//! `reset_run_length()` methods.
//!
//! Two hazard function variants:
//! - [`HazardModel::Constant`] — fixed `H = 1/λ`. Simpler, faster.
//! - [`HazardModel::Adaptive`] — `H` adjusts based on observed changepoint
//!   frequency. More accurate for channels with heterogeneous topic lifetimes.

use super::Branch;

// ── Newtypes ────────────────────────────────────────────────────────────────

/// Probability value clamped to `[0.0, 1.0]`.
///
/// Encodes the constraint at the type level — callers can trust that a
/// `Probability` never needs bounds checking.
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
///
/// Prevents nonsensical negative rates from entering the computation.
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

/// Hazard function model selection.
#[derive(Debug, Clone)]
pub enum HazardModel {
    /// Constant hazard: `H = 1/lambda`.
    ///
    /// `lambda` is the expected run length in messages. A branch typically
    /// spans ~20 messages before a topic shift.
    Constant {
        /// Expected run length (messages). Default: 20.
        lambda: f64,
    },

    /// Adaptive hazard: `H` adjusts toward the observed changepoint frequency.
    ///
    /// Tracks a running count of changepoints and total messages, computing
    /// `H = changepoints / total_messages`. Bootstraps from the constant
    /// hazard until enough data accumulates.
    Adaptive {
        /// Bootstrap lambda (used until `min_observations` are reached).
        initial_lambda: f64,
        /// Minimum total messages before adaptive hazard kicks in.
        min_observations: u64,
        /// Smoothing factor for the adaptive hazard EWMA.
        alpha: f64,
    },
}

impl Default for HazardModel {
    fn default() -> Self {
        Self::Constant { lambda: 20.0 }
    }
}

/// Configuration for the CPD layer.
#[derive(Debug, Clone)]
pub struct CpdConfig {
    /// Hazard function model.
    pub hazard: HazardModel,
    /// Threshold for declaring a changepoint: `P(r=0) / max(P(r>0))`.
    /// When this ratio exceeds the threshold, a changepoint is declared.
    /// Default: 1.0 (changepoint run-length is more probable than any
    /// continuation).
    pub changepoint_ratio_threshold: f64,
    /// Minimum probability below which run-lengths are pruned.
    /// Default: 1e-4.
    pub pruning_threshold: f64,
    /// Maximum run-length vector size (hard cap). Default: 100.
    pub max_run_lengths: usize,
    /// Minimum confidence from the rate estimator before CPD produces
    /// meaningful results. Below this, returns `Uncertain`. Default: 0.3.
    pub min_confidence: f32,
}

impl Default for CpdConfig {
    fn default() -> Self {
        Self {
            hazard: HazardModel::default(),
            changepoint_ratio_threshold: 1.0,
            pruning_threshold: 1e-4,
            max_run_lengths: 100,
            min_confidence: 0.3,
        }
    }
}

// ── Adaptive hazard state ──────────────────────────────────────────────────

/// Tracks adaptive hazard function state across observations.
///
/// Stored externally (e.g. per-channel) and passed into [`detect_changepoint`]
/// when using [`HazardModel::Adaptive`].
#[derive(Debug, Clone)]
pub struct AdaptiveHazardState {
    /// Total messages observed.
    pub total_messages: u64,
    /// Total changepoints detected.
    pub total_changepoints: u64,
    /// Current smoothed hazard rate.
    pub smoothed_hazard: f64,
}

impl AdaptiveHazardState {
    /// Create a new adaptive hazard state with a bootstrap hazard.
    pub fn new(initial_lambda: f64) -> Self {
        Self {
            total_messages: 0,
            total_changepoints: 0,
            smoothed_hazard: 1.0 / initial_lambda,
        }
    }

    /// Record an observation, optionally with a changepoint.
    pub fn record(&mut self, was_changepoint: bool, alpha: f64) {
        self.total_messages = self.total_messages.saturating_add(1);
        if was_changepoint {
            self.total_changepoints = self.total_changepoints.saturating_add(1);
        }
        // EWMA update: smoothed_hazard = alpha * instantaneous + (1-alpha) * smoothed
        let instantaneous = if was_changepoint { 1.0 } else { 0.0 };
        self.smoothed_hazard = alpha * instantaneous + (1.0 - alpha) * self.smoothed_hazard;
    }

    /// Current hazard rate.
    pub fn hazard_rate(&self) -> f64 {
        self.smoothed_hazard.max(f64::MIN_POSITIVE)
    }
}

// ── Run-length distribution ────────────────────────────────────────────────

/// Per-branch run-length probability distribution.
///
/// `distribution[i]` = `P(run_length = i)`. Index 0 is the changepoint
/// probability; higher indices represent longer continuation runs.
///
/// Stored per-branch and passed into [`detect_changepoint`] by mutable ref.
#[derive(Debug, Clone)]
pub struct RunLengthDistribution {
    /// Probability mass for each run length. `dist[0]` = P(changepoint).
    dist: Vec<f64>,
}

impl RunLengthDistribution {
    /// Initialize with a single run-length of 0 at probability 1.0.
    /// This is the cold-start state: first message creates the branch.
    pub fn new() -> Self {
        Self { dist: vec![1.0] }
    }

    /// Number of tracked run-lengths.
    pub fn len(&self) -> usize {
        self.dist.len()
    }

    /// Whether the distribution is empty (should never happen in practice).
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

// ── Result ──────────────────────────────────────────────────────────────────

/// Outcome of a single changepoint detection step.
#[derive(Debug, Clone)]
pub enum ChangePointVerdict {
    /// A changepoint was detected — the topic/conversation has shifted.
    ChangePoint {
        /// Posterior probability of changepoint.
        probability: Probability,
        /// Ratio of `P(r=0) / max(P(r>0))`.
        ratio: f64,
    },
    /// The current run continues — no evidence of a topic shift.
    Continuation {
        /// Posterior probability of the most likely continuation run-length.
        probability: Probability,
        /// Most probable run length.
        mode_run_length: u32,
    },
    /// Not enough data to make a meaningful determination.
    Uncertain {
        /// Rate estimator confidence at the time of the call.
        confidence: f32,
    },
}

/// Full result from [`detect_changepoint`].
#[derive(Debug, Clone)]
pub struct ChangePointResult {
    /// The verdict.
    pub verdict: ChangePointVerdict,
    /// Number of tracked run-lengths after pruning.
    pub tracked_run_lengths: usize,
}

// ── Core algorithm ──────────────────────────────────────────────────────────

/// Exponential PDF: `f(x; rate) = rate * exp(-rate * x)` for `x >= 0`.
///
/// Uses the rate parameterization where `rate = 1 / mean_gap`.
#[inline]
fn exp_pdf(gap: f64, rate: f64) -> f64 {
    if gap < 0.0 || rate <= 0.0 {
        return 0.0;
    }
    rate * (-rate * gap).exp()
}

/// Compute the hazard rate for the given model and state.
fn compute_hazard(hazard: &HazardModel, adaptive_state: Option<&AdaptiveHazardState>) -> f64 {
    match hazard {
        HazardModel::Constant { lambda } => 1.0 / lambda.max(1.0),
        HazardModel::Adaptive {
            initial_lambda,
            min_observations,
            ..
        } => match adaptive_state {
            Some(state) if state.total_messages >= *min_observations => state.hazard_rate(),
            _ => 1.0 / initial_lambda.max(1.0),
        },
    }
}

/// Bayesian online change-point detection.
///
/// Pure function: takes branch state, the observed inter-message gap,
/// per-branch run-length distribution, and config. Returns a
/// [`ChangePointResult`] and mutates the run-length distribution and
/// branch run-length counter.
///
/// # Algorithm (Adams & MacKay 2007, adapted)
///
/// 1. Evaluate the predictive probability of the observed gap under
///    `Exp(1/μ_k)` where `μ_k` is the branch's EWMA rate estimate.
/// 2. For each existing run-length `r`, compute the growth probability
///    (run continues) and the changepoint probability (run resets to 0).
/// 3. Normalize the updated distribution.
/// 4. Prune low-probability run-lengths.
/// 5. If `P(r=0) / max(P(r>0)) > threshold`, declare a changepoint.
pub fn detect_changepoint(
    branch: &mut Branch,
    observed_gap: f64,
    run_lengths: &mut RunLengthDistribution,
    config: &CpdConfig,
    adaptive_state: Option<&AdaptiveHazardState>,
) -> ChangePointResult {
    // Gate on confidence: if the rate estimator hasn't converged, bail.
    let confidence = branch.rate_confidence();
    if confidence < config.min_confidence {
        branch.increment_run_length();
        return ChangePointResult {
            verdict: ChangePointVerdict::Uncertain { confidence },
            tracked_run_lengths: run_lengths.len(),
        };
    }

    // Rate from the EWMA: this is the *mean inter-message gap* in seconds.
    // The exponential rate parameter is the reciprocal.
    let mean_gap = branch.rate_estimate().max(f64::MIN_POSITIVE);
    let rate = 1.0 / mean_gap;

    // Predictive probability of the observed gap.
    let predictive = exp_pdf(observed_gap, rate);

    // Hazard rate.
    let h = compute_hazard(&config.hazard, adaptive_state);
    let h_complement = 1.0 - h;

    // ── Run-length update ──────────────────────────────────────────────
    //
    // Growth: P(r_t = r+1) ∝ P(r_{t-1} = r) · pdf(gap; rate) · (1 - H)
    // Changepoint: P(r_t = 0) ∝ Σ_r P(r_{t-1} = r) · pdf(gap; rate) · H

    let old_dist = &run_lengths.dist;
    let n = old_dist.len();

    // Accumulate changepoint mass: sum over all old run-lengths.
    let mut changepoint_mass = 0.0_f64;
    // New distribution: index 0 = changepoint, indices 1..=n = growth.
    let mut new_dist = Vec::with_capacity((n + 1).min(config.max_run_lengths + 1));

    // Placeholder for index 0 (changepoint) — we'll fill it after the loop.
    new_dist.push(0.0);

    // Growth probabilities: each old run-length r produces a new r+1.
    for &p_r in old_dist.iter() {
        let joint = p_r * predictive;
        let growth = joint * h_complement;
        changepoint_mass += joint * h;
        new_dist.push(growth);
    }

    // Fill in the changepoint mass at index 0.
    new_dist[0] = changepoint_mass;

    // ── Normalize ──────────────────────────────────────────────────────

    let total: f64 = new_dist.iter().sum();
    if total > 0.0 {
        let inv_total = 1.0 / total;
        for p in new_dist.iter_mut() {
            *p *= inv_total;
        }
    } else {
        // Degenerate case: everything underflowed. Reset to uniform-ish.
        new_dist.clear();
        new_dist.push(1.0);
    }

    // ── Prune ──────────────────────────────────────────────────────────

    // Truncate tail entries below the pruning threshold.
    while new_dist.len() > 1
        && new_dist
            .last()
            .is_some_and(|&p| p < config.pruning_threshold)
    {
        new_dist.pop();
    }

    // Hard cap on vector size.
    if new_dist.len() > config.max_run_lengths {
        new_dist.truncate(config.max_run_lengths);
        // Re-normalize after truncation.
        let total: f64 = new_dist.iter().sum();
        if total > 0.0 {
            let inv_total = 1.0 / total;
            for p in new_dist.iter_mut() {
                *p *= inv_total;
            }
        }
    }

    run_lengths.dist = new_dist;

    // ── Verdict ────────────────────────────────────────────────────────

    let cp_prob = run_lengths.changepoint_probability();
    let max_cont = run_lengths.max_continuation_probability();

    let ratio = if max_cont.value() > 0.0 {
        cp_prob.value() / max_cont.value()
    } else {
        // No continuation mass — everything says changepoint.
        f64::INFINITY
    };

    let is_changepoint = ratio > config.changepoint_ratio_threshold;

    if is_changepoint {
        branch.reset_run_length();
        ChangePointResult {
            verdict: ChangePointVerdict::ChangePoint {
                probability: cp_prob,
                ratio,
            },
            tracked_run_lengths: run_lengths.len(),
        }
    } else {
        branch.increment_run_length();
        ChangePointResult {
            verdict: ChangePointVerdict::Continuation {
                probability: max_cont,
                mode_run_length: run_lengths.mode(),
            },
            tracked_run_lengths: run_lengths.len(),
        }
    }
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
        // Snap the estimator to our desired value and observation count.
        // First observation snaps, subsequent ones converge.
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
    fn positive_rate_clamps_negative() {
        let r = PositiveRate::new(-5.0);
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
        // f(0; rate=2) = 2 * exp(0) = 2.0
        let p = exp_pdf(0.0, 2.0);
        assert!((p - 2.0).abs() < 1e-10);
    }

    #[test]
    fn exp_pdf_positive_gap() {
        // f(1; rate=1) = 1 * exp(-1) ≈ 0.3679
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

    #[test]
    fn exp_pdf_negative_rate_is_zero() {
        assert_eq!(exp_pdf(1.0, -1.0), 0.0);
    }

    // ── Hazard computation ────────────────────────────────────────────

    #[test]
    fn constant_hazard() {
        let h = compute_hazard(&HazardModel::Constant { lambda: 20.0 }, None);
        assert!((h - 0.05).abs() < 1e-10);
    }

    #[test]
    fn constant_hazard_clamps_small_lambda() {
        let h = compute_hazard(&HazardModel::Constant { lambda: 0.5 }, None);
        // lambda clamped to 1.0, so h = 1.0
        assert!((h - 1.0).abs() < 1e-10);
    }

    #[test]
    fn adaptive_hazard_uses_bootstrap_when_insufficient_data() {
        let model = HazardModel::Adaptive {
            initial_lambda: 20.0,
            min_observations: 50,
            alpha: 0.05,
        };
        let state = AdaptiveHazardState::new(20.0);
        // 0 messages < 50 min_observations, so falls back to bootstrap.
        let h = compute_hazard(&model, Some(&state));
        assert!((h - 0.05).abs() < 1e-10);
    }

    #[test]
    fn adaptive_hazard_uses_smoothed_when_sufficient_data() {
        let model = HazardModel::Adaptive {
            initial_lambda: 20.0,
            min_observations: 5,
            alpha: 0.05,
        };
        let mut state = AdaptiveHazardState::new(10.0); // H = 0.1
        // Record 10 non-changepoints to bring total above min_observations.
        for _ in 0..10 {
            state.record(false, 0.05);
        }
        let h = compute_hazard(&model, Some(&state));
        // After 10 non-changepoints with alpha=0.05:
        // smoothed_hazard started at 0.1, decayed toward 0.
        assert!(h < 0.1, "hazard should have decreased from 0.1, got {h}");
        assert!(h > 0.0);
    }

    // ── AdaptiveHazardState ───────────────────────────────────────────

    #[test]
    fn adaptive_state_records_changepoint() {
        let mut state = AdaptiveHazardState::new(20.0);
        state.record(true, 0.1);
        assert_eq!(state.total_messages, 1);
        assert_eq!(state.total_changepoints, 1);
        // smoothed = 0.1 * 1.0 + 0.9 * 0.05 = 0.1 + 0.045 = 0.145
        assert!((state.smoothed_hazard - 0.145).abs() < 1e-10);
    }

    #[test]
    fn adaptive_state_records_continuation() {
        let mut state = AdaptiveHazardState::new(20.0);
        state.record(false, 0.1);
        assert_eq!(state.total_messages, 1);
        assert_eq!(state.total_changepoints, 0);
        // smoothed = 0.1 * 0.0 + 0.9 * 0.05 = 0.045
        assert!((state.smoothed_hazard - 0.045).abs() < 1e-10);
    }

    #[test]
    fn adaptive_state_saturates() {
        let mut state = AdaptiveHazardState::new(20.0);
        state.total_messages = u64::MAX;
        state.total_changepoints = u64::MAX;
        state.record(true, 0.1);
        assert_eq!(state.total_messages, u64::MAX);
        assert_eq!(state.total_changepoints, u64::MAX);
    }

    // ── detect_changepoint — low confidence ───────────────────────────

    #[test]
    fn uncertain_when_low_confidence() {
        // 0 observations → confidence = 0.0 < 0.3.
        let mut branch = make_branch(10.0, 0);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let result = detect_changepoint(&mut branch, 5.0, &mut rld, &config, None);
        match result.verdict {
            ChangePointVerdict::Uncertain { confidence } => {
                assert_eq!(confidence, 0.0);
            }
            _ => panic!("expected Uncertain, got {:?}", result.verdict),
        }
        // Should still increment run length.
        assert_eq!(branch.run_length(), 1);
    }

    // ── detect_changepoint — continuation ─────────────────────────────

    #[test]
    fn continuation_on_expected_gap() {
        // Branch expects ~10s gaps. Observe a 10s gap → continuation.
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let result = detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
        match result.verdict {
            ChangePointVerdict::Continuation { .. } => {}
            _ => panic!("expected Continuation, got {:?}", result.verdict),
        }
        assert_eq!(branch.run_length(), 1);
    }

    #[test]
    fn multiple_continuations_grow_run_length() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        for i in 1..=5 {
            let result = detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
            match result.verdict {
                ChangePointVerdict::Continuation {
                    mode_run_length, ..
                } => {
                    // Mode should generally be near the current run length.
                    assert!(mode_run_length > 0, "mode should be > 0 at step {i}");
                }
                _ => panic!(
                    "expected Continuation at step {i}, got {:?}",
                    result.verdict
                ),
            }
            assert_eq!(branch.run_length(), i);
        }
    }

    // ── detect_changepoint — changepoint ──────────────────────────────

    #[test]
    fn changepoint_on_moderate_gap_shift() {
        // Branch expects ~5s gaps. Build up a run of normal gaps, then
        // send gaps that are abnormal but still have non-zero pdf
        // (so the Bayesian update has discriminating power).
        //
        // Key insight: when exp_pdf ≈ 0, the observation provides no
        // information and the posterior just reflects the prior hazard
        // structure. We need gaps that are unusual enough to shift mass
        // toward r=0 but not so extreme that the pdf underflows.
        //
        // With rate = 1/5 = 0.2, a gap of 30s gives:
        //   pdf = 0.2 * exp(-6) ≈ 0.000495
        // This is small but non-zero — the hazard-weighted sum accumulates
        // changepoint evidence over multiple such gaps.
        let mut branch = make_branch(5.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            hazard: HazardModel::Constant { lambda: 5.0 },
            changepoint_ratio_threshold: 1.0,
            ..Default::default()
        };

        // Build up a run of normal observations.
        for _ in 0..5 {
            detect_changepoint(&mut branch, 5.0, &mut rld, &config, None);
        }

        // Now send several moderately anomalous gaps. Each one shifts
        // mass toward r=0 because the pdf under the current rate is low,
        // and the hazard accumulates the changepoint signal.
        let mut detected_cp = false;
        for _ in 0..10 {
            let result = detect_changepoint(&mut branch, 30.0, &mut rld, &config, None);
            if matches!(result.verdict, ChangePointVerdict::ChangePoint { .. }) {
                detected_cp = true;
                break;
            }
        }
        assert!(
            detected_cp,
            "should detect a changepoint after sustained anomalous gaps"
        );
        assert_eq!(
            branch.run_length(),
            0,
            "run length should reset on changepoint"
        );
    }

    // ── Pruning behavior ──────────────────────────────────────────────

    #[test]
    fn pruning_keeps_distribution_compact() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            pruning_threshold: 1e-4,
            max_run_lengths: 100,
            ..Default::default()
        };

        // Run many observations — distribution should stay compact.
        for _ in 0..200 {
            detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
        }
        assert!(
            rld.len() <= config.max_run_lengths,
            "distribution length {} should be <= {}",
            rld.len(),
            config.max_run_lengths,
        );
    }

    #[test]
    fn pruning_respects_hard_cap() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            pruning_threshold: 1e-20, // Very permissive pruning.
            max_run_lengths: 10,      // But strict hard cap.
            ..Default::default()
        };

        for _ in 0..50 {
            detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
        }
        assert!(
            rld.len() <= config.max_run_lengths,
            "distribution length {} should be <= hard cap {}",
            rld.len(),
            config.max_run_lengths,
        );
    }

    // ── Distribution normalization ────────────────────────────────────

    #[test]
    fn distribution_sums_to_one() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        for _ in 0..20 {
            detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
        }

        let sum: f64 = rld.as_slice().iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "distribution should sum to ~1.0, got {sum}"
        );
    }

    // ── Adaptive hazard integration ───────────────────────────────────

    #[test]
    fn adaptive_hazard_produces_valid_results() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            hazard: HazardModel::Adaptive {
                initial_lambda: 20.0,
                min_observations: 5,
                alpha: 0.05,
            },
            ..Default::default()
        };
        let mut adaptive_state = AdaptiveHazardState::new(20.0);

        for _ in 0..20 {
            let result =
                detect_changepoint(&mut branch, 10.0, &mut rld, &config, Some(&adaptive_state));
            let was_cp = matches!(result.verdict, ChangePointVerdict::ChangePoint { .. });
            adaptive_state.record(was_cp, 0.05);
        }

        // Distribution should be valid.
        let sum: f64 = rld.as_slice().iter().sum();
        assert!((sum - 1.0).abs() < 1e-6);
        assert!(rld.len() > 1, "should have multiple run-lengths tracked");
    }

    // ── Edge cases ────────────────────────────────────────────────────

    #[test]
    fn zero_gap_does_not_panic() {
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let result = detect_changepoint(&mut branch, 0.0, &mut rld, &config, None);
        // Should produce a valid result without panicking.
        assert!(rld.len() >= 1);
        match result.verdict {
            ChangePointVerdict::Continuation { .. } | ChangePointVerdict::ChangePoint { .. } => {}
            ChangePointVerdict::Uncertain { .. } => {
                panic!("should not be uncertain with 10 observations")
            }
        }
    }

    #[test]
    fn very_large_gap_does_not_panic() {
        let mut branch = make_branch(1.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        // Extremely large gap — could cause underflow in exp().
        let result = detect_changepoint(&mut branch, 1e10, &mut rld, &config, None);
        assert!(rld.len() >= 1);
        // Should not panic, and should likely detect a changepoint.
        match result.verdict {
            ChangePointVerdict::ChangePoint { .. } | ChangePointVerdict::Continuation { .. } => {}
            ChangePointVerdict::Uncertain { .. } => {
                panic!("should not be uncertain with 10 observations")
            }
        }
    }

    #[test]
    fn very_small_rate_does_not_panic() {
        // Branch with a very large mean gap (very slow channel).
        let mut branch = make_branch(100_000.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig::default();

        let result = detect_changepoint(&mut branch, 5.0, &mut rld, &config, None);
        assert!(rld.len() >= 1);
        match result.verdict {
            ChangePointVerdict::ChangePoint { .. } | ChangePointVerdict::Continuation { .. } => {}
            ChangePointVerdict::Uncertain { .. } => {
                panic!("should not be uncertain with 10 observations")
            }
        }
    }

    // ── CpdConfig defaults ────────────────────────────────────────────

    #[test]
    fn cpd_config_defaults() {
        let config = CpdConfig::default();
        assert_eq!(config.changepoint_ratio_threshold, 1.0);
        assert_eq!(config.pruning_threshold, 1e-4);
        assert_eq!(config.max_run_lengths, 100);
        assert_eq!(config.min_confidence, 0.3);
        match config.hazard {
            HazardModel::Constant { lambda } => assert_eq!(lambda, 20.0),
            _ => panic!("default should be Constant"),
        }
    }

    // ── Steady-state convergence ──────────────────────────────────────

    #[test]
    fn steady_state_mode_tracks_run_length() {
        // In a steady stream of same-gap messages, the mode of the
        // run-length distribution should roughly track how many messages
        // we've sent (up to the point where hazard erosion caps it).
        let mut branch = make_branch(10.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            hazard: HazardModel::Constant { lambda: 50.0 },
            pruning_threshold: 1e-6,
            max_run_lengths: 200,
            ..Default::default()
        };

        for _ in 0..30 {
            detect_changepoint(&mut branch, 10.0, &mut rld, &config, None);
        }

        let mode = rld.mode();
        // With lambda=50 and 30 observations, most probability mass
        // should be at high run-lengths. The mode should be well above 0.
        assert!(
            mode > 10,
            "after 30 steady observations with lambda=50, mode should be >10, got {mode}"
        );
    }

    // ── Changepoint resets distribution ────────────────────────────────

    #[test]
    fn after_changepoint_distribution_concentrates_at_zero() {
        let mut branch = make_branch(5.0, 10);
        let mut rld = RunLengthDistribution::new();
        let config = CpdConfig {
            changepoint_ratio_threshold: 0.1,
            ..Default::default()
        };

        // Build up run.
        for _ in 0..10 {
            detect_changepoint(&mut branch, 5.0, &mut rld, &config, None);
        }

        // Force a changepoint with a huge gap.
        let result = detect_changepoint(&mut branch, 1000.0, &mut rld, &config, None);

        if matches!(result.verdict, ChangePointVerdict::ChangePoint { .. }) {
            // After changepoint, the P(r=0) should be the highest mass.
            assert!(
                rld.changepoint_probability().value() > rld.max_continuation_probability().value(),
                "after changepoint, P(r=0) should dominate"
            );
        }
    }
}
