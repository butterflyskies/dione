//! Rate estimation implementations.
//!
//! Two variants behind the [`RateEstimator`] trait:
//!
//! - [`EwmaEstimator`] (Variant 1): classic alpha-smoothed EWMA.
//! - [`WindowedMedianEstimator`] (Variant 2): rolling window median, robust
//!   to outliers. Same interface, no smoothing parameter.

use super::RateEstimator;

// ── Variant 1: Classic EWMA ─────────────────────────────────────────────────

/// Exponentially weighted moving average rate estimator.
///
/// Tracks the expected inter-message gap in seconds. Comparable to TCP RTT
/// smoothing (RFC 6298) with a bias toward reactivity (default alpha=0.2
/// vs TCP's 0.125).
///
/// # Convergence
///
/// With alpha=0.2, ~5-message effective window (1/alpha). 90% convergence
/// in ~11 messages on a rate shift (e.g. 60s -> 5s gaps).
#[derive(Debug, Clone)]
pub struct EwmaEstimator {
    /// Smoothing factor in `(0.0, 1.0]`. Higher = more reactive.
    alpha: f64,
    /// Current smoothed estimate (seconds).
    estimate: f64,
    /// Number of gap observations recorded.
    observations: u32,
}

impl EwmaEstimator {
    /// Create a new EWMA estimator.
    ///
    /// # Arguments
    /// - `alpha`: smoothing factor in `(0.0, 1.0]`
    /// - `initial_estimate`: bootstrap estimate (seconds) used until the
    ///   first observation snaps the value.
    pub fn new(alpha: f64, initial_estimate: f64) -> Self {
        debug_assert!((0.0..=1.0).contains(&alpha), "alpha must be in (0.0, 1.0]");
        Self {
            alpha,
            estimate: initial_estimate,
            observations: 0,
        }
    }
}

impl RateEstimator for EwmaEstimator {
    fn estimate(&self) -> f64 {
        self.estimate
    }

    fn confidence(&self) -> f32 {
        (self.observations as f32 / 10.0).min(1.0)
    }

    fn observation_count(&self) -> u32 {
        self.observations
    }

    fn observe(&mut self, gap_secs: f64) {
        if self.observations == 0 {
            // First observation snaps (effectively alpha=1.0).
            self.estimate = gap_secs;
        } else {
            // EWMA: estimate = alpha * observed + (1 - alpha) * previous
            self.estimate = self.alpha * gap_secs + (1.0 - self.alpha) * self.estimate;
        }
        self.observations = self.observations.saturating_add(1);
    }

    fn observe_boosted(&mut self, gap_secs: f64, alpha_boost: f64) {
        let boosted_alpha = (self.alpha * alpha_boost).min(1.0);
        if self.observations == 0 {
            self.estimate = gap_secs;
        } else {
            self.estimate = boosted_alpha * gap_secs + (1.0 - boosted_alpha) * self.estimate;
        }
        self.observations = self.observations.saturating_add(1);
    }
}

// ── Variant 2: Windowed Median ──────────────────────────────────────────────

/// Rolling window median rate estimator. No smoothing parameter — robust to
/// outliers by construction.
///
/// Maintains a fixed-size window of recent observations and returns their
/// median as the rate estimate. Insertion is O(1), median is O(n log n)
/// but n is small (default window=11).
#[derive(Debug, Clone)]
pub struct WindowedMedianEstimator {
    /// Ring buffer of recent gap observations.
    window: Vec<f64>,
    /// Maximum window size.
    capacity: usize,
    /// Write cursor (next insertion index in the ring buffer).
    cursor: usize,
    /// Total observations ever recorded (may exceed window capacity).
    observations: u32,
    /// Bootstrap estimate used when window is empty.
    initial_estimate: f64,
}

impl WindowedMedianEstimator {
    /// Create a new windowed median estimator.
    ///
    /// # Arguments
    /// - `window_size`: number of recent observations to keep (should be odd
    ///   for a unique median). Default: 11 (matching EWMA's ~5-message
    ///   effective window, doubled for robustness).
    /// - `initial_estimate`: bootstrap estimate (seconds).
    pub fn new(window_size: usize, initial_estimate: f64) -> Self {
        debug_assert!(window_size > 0, "window_size must be > 0");
        Self {
            window: Vec::with_capacity(window_size),
            capacity: window_size,
            cursor: 0,
            observations: 0,
            initial_estimate,
        }
    }

    /// Compute median of the current window. Returns `initial_estimate` if empty.
    fn compute_median(&self) -> f64 {
        if self.window.is_empty() {
            return self.initial_estimate;
        }

        let mut sorted: Vec<f64> = self.window.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mid = sorted.len() / 2;
        if sorted.len().is_multiple_of(2) {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        } else {
            sorted[mid]
        }
    }

    /// Insert a value into the ring buffer.
    fn push(&mut self, value: f64) {
        if self.window.len() < self.capacity {
            self.window.push(value);
        } else {
            self.window[self.cursor] = value;
        }
        self.cursor = (self.cursor + 1) % self.capacity;
    }
}

impl RateEstimator for WindowedMedianEstimator {
    fn estimate(&self) -> f64 {
        self.compute_median()
    }

    fn confidence(&self) -> f32 {
        (self.observations as f32 / 10.0).min(1.0)
    }

    fn observation_count(&self) -> u32 {
        self.observations
    }

    fn observe(&mut self, gap_secs: f64) {
        self.push(gap_secs);
        self.observations = self.observations.saturating_add(1);
    }

    fn observe_boosted(&mut self, gap_secs: f64, _alpha_boost: f64) {
        // Windowed median has no alpha to boost — just observe normally.
        // The outlier robustness of median makes boosting unnecessary.
        self.push(gap_secs);
        self.observations = self.observations.saturating_add(1);
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EWMA tests ──────────────────────────────────────────────────────

    #[test]
    fn ewma_initial_estimate() {
        let e = EwmaEstimator::new(0.2, 60.0);
        assert_eq!(e.estimate(), 60.0);
        assert_eq!(e.confidence(), 0.0);
        assert_eq!(e.observation_count(), 0);
    }

    #[test]
    fn ewma_first_observation_snaps() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        e.observe(5.0);
        assert_eq!(e.estimate(), 5.0);
        assert_eq!(e.observation_count(), 1);
        assert!((e.confidence() - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn ewma_smoothing_converges() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        e.observe(10.0);
        assert_eq!(e.estimate(), 10.0);

        for _ in 0..20 {
            e.observe(5.0);
        }
        assert!(
            (e.estimate() - 5.0).abs() < 0.1,
            "expected ~5.0, got {}",
            e.estimate()
        );
    }

    #[test]
    fn ewma_formula_correctness() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        e.observe(10.0); // Snaps to 10.0.
        e.observe(20.0); // 0.2 * 20 + 0.8 * 10 = 4 + 8 = 12.
        assert!(
            (e.estimate() - 12.0).abs() < f64::EPSILON,
            "expected 12.0, got {}",
            e.estimate()
        );
        e.observe(20.0); // 0.2 * 20 + 0.8 * 12 = 4 + 9.6 = 13.6.
        assert!(
            (e.estimate() - 13.6).abs() < 1e-10,
            "expected 13.6, got {}",
            e.estimate()
        );
    }

    #[test]
    fn ewma_confidence_ramp() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        for i in 1..=15 {
            e.observe(5.0);
            let expected = (i as f32 / 10.0).min(1.0);
            assert!(
                (e.confidence() - expected).abs() < f32::EPSILON,
                "at observation {i}: expected {expected}, got {}",
                e.confidence()
            );
        }
    }

    #[test]
    fn ewma_boosted_alpha() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        e.observe(10.0);
        e.observe_boosted(20.0, 1.5);
        assert!(
            (e.estimate() - 13.0).abs() < f64::EPSILON,
            "expected 13.0, got {}",
            e.estimate()
        );
    }

    #[test]
    fn ewma_boosted_alpha_clamped() {
        let mut e = EwmaEstimator::new(0.8, 60.0);
        e.observe(10.0);
        e.observe_boosted(20.0, 2.0);
        assert!(
            (e.estimate() - 20.0).abs() < f64::EPSILON,
            "expected 20.0 (full snap), got {}",
            e.estimate()
        );
    }

    #[test]
    fn ewma_saturating_observation_count() {
        let mut e = EwmaEstimator::new(0.2, 60.0);
        e.observations = u32::MAX - 1;
        e.observe(5.0);
        assert_eq!(e.observation_count(), u32::MAX);
        e.observe(5.0);
        assert_eq!(e.observation_count(), u32::MAX);
    }

    // ── Windowed median tests ───────────────────────────────────────────

    #[test]
    fn median_initial_estimate() {
        let m = WindowedMedianEstimator::new(11, 60.0);
        assert_eq!(m.estimate(), 60.0);
        assert_eq!(m.confidence(), 0.0);
        assert_eq!(m.observation_count(), 0);
    }

    #[test]
    fn median_single_observation() {
        let mut m = WindowedMedianEstimator::new(11, 60.0);
        m.observe(5.0);
        assert_eq!(m.estimate(), 5.0);
    }

    #[test]
    fn median_odd_window() {
        let mut m = WindowedMedianEstimator::new(5, 60.0);
        for &v in &[10.0, 5.0, 8.0, 3.0, 7.0] {
            m.observe(v);
        }
        // Sorted: [3, 5, 7, 8, 10]. Median = 7.
        assert_eq!(m.estimate(), 7.0);
    }

    #[test]
    fn median_even_count() {
        let mut m = WindowedMedianEstimator::new(11, 60.0);
        for &v in &[10.0, 5.0, 8.0, 3.0] {
            m.observe(v);
        }
        assert_eq!(m.estimate(), 6.5);
    }

    #[test]
    fn median_outlier_robustness() {
        let mut m = WindowedMedianEstimator::new(11, 60.0);
        for _ in 0..10 {
            m.observe(5.0);
        }
        m.observe(500.0);
        assert_eq!(m.estimate(), 5.0);
    }

    #[test]
    fn median_ring_buffer_eviction() {
        let mut m = WindowedMedianEstimator::new(3, 60.0);
        m.observe(10.0);
        m.observe(20.0);
        m.observe(30.0);
        assert_eq!(m.estimate(), 20.0);

        m.observe(5.0);
        assert_eq!(m.estimate(), 20.0);

        m.observe(7.0);
        assert_eq!(m.estimate(), 7.0);
    }

    #[test]
    fn median_confidence_ramp() {
        let mut m = WindowedMedianEstimator::new(11, 60.0);
        for i in 1..=15 {
            m.observe(5.0);
            let expected = (i as f32 / 10.0).min(1.0);
            assert!(
                (m.confidence() - expected).abs() < f32::EPSILON,
                "at observation {i}: expected {expected}, got {}",
                m.confidence()
            );
        }
    }

    #[test]
    fn median_boosted_is_normal() {
        let mut m = WindowedMedianEstimator::new(5, 60.0);
        m.observe(10.0);
        m.observe_boosted(20.0, 1.5);
        assert_eq!(m.estimate(), 15.0);
        assert_eq!(m.observation_count(), 2);
    }

    #[test]
    fn median_saturating_observation_count() {
        let mut m = WindowedMedianEstimator::new(3, 60.0);
        m.observations = u32::MAX - 1;
        m.observe(5.0);
        assert_eq!(m.observation_count(), u32::MAX);
        m.observe(5.0);
        assert_eq!(m.observation_count(), u32::MAX);
    }

    // ── Cross-variant property tests ────────────────────────────────────

    #[test]
    fn both_variants_share_confidence_scale() {
        let mut ewma = EwmaEstimator::new(0.2, 60.0);
        let mut median = WindowedMedianEstimator::new(11, 60.0);

        for i in 0..15 {
            let gap = 5.0 + (i as f64);
            ewma.observe(gap);
            median.observe(gap);
            assert_eq!(
                ewma.confidence(),
                median.confidence(),
                "confidence diverged at observation {i}"
            );
        }
    }

    #[test]
    fn both_variants_snap_on_first() {
        let mut ewma = EwmaEstimator::new(0.2, 60.0);
        let mut median = WindowedMedianEstimator::new(11, 60.0);

        ewma.observe(42.0);
        median.observe(42.0);

        assert_eq!(ewma.estimate(), 42.0);
        assert_eq!(median.estimate(), 42.0);
    }
}
