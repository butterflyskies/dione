//! Branch feature vector: scores incoming messages against active conversation
//! branches to classify which branch a message belongs to.
//!
//! Two scoring variants are provided:
//! - **Variant A** (weighted linear): normalizes by the weights of present
//!   signals only, so absent signals don't dilute the score.
//! - **Variant B** (rule-based cascade): reply chain is deterministic, then
//!   topic+temporal, with participant overlap as a tiebreaker.

use std::fmt;

use super::BranchId;

// ── Embedding trait ──────────────────────────────────────────────────────────

/// Trait for text embedding and similarity computation.
///
/// Abstracted so callers can plug in a real model or a test mock.
pub trait Embedder {
    /// Produces a dense embedding vector for the given text.
    fn embed(&self, text: &str) -> Vec<f32>;

    /// Computes cosine similarity between two embedding vectors.
    ///
    /// Returns 0.0 for zero-magnitude vectors.
    fn cosine_similarity(&self, a: &[f32], b: &[f32]) -> f64 {
        if a.len() != b.len() {
            return 0.0;
        }

        let mut dot = 0.0_f64;
        let mut mag_a = 0.0_f64;
        let mut mag_b = 0.0_f64;

        for (&x, &y) in a.iter().zip(b.iter()) {
            let x = f64::from(x);
            let y = f64::from(y);
            dot += x * y;
            mag_a += x * x;
            mag_b += y * y;
        }

        let denom = mag_a.sqrt() * mag_b.sqrt();
        if denom == 0.0 { 0.0 } else { dot / denom }
    }
}

// ── Feature vector ───────────────────────────────────────────────────────────

/// Raw signal strengths for scoring a message against a single branch.
///
/// Each field is `Option<f64>` — `None` means the signal is absent (e.g. no
/// reply chain exists), and its weight is excluded from normalization.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureVector {
    /// 1.0 if the message replies to a message in this branch, 0.0 otherwise.
    pub reply_chain: Option<f64>,
    /// Cosine similarity between the message and the branch's topic centroid.
    pub topic_similarity: Option<f64>,
    /// Fraction of the message's mentioned/active participants who are active
    /// in this branch (0.0..=1.0).
    pub participant_overlap: Option<f64>,
    /// Temporal proximity: decays from 1.0 as the time since the branch's last
    /// message increases.
    pub temporal_proximity: Option<f64>,
    /// 1.0 if the message mentions a user active in this branch, 0.0 otherwise.
    pub mention_match: Option<f64>,
}

impl FeatureVector {
    /// Returns an iterator over `(signal_value, weight)` pairs for present
    /// signals.
    fn present_signals(&self, weights: &ScoringWeights) -> Vec<(f64, f64)> {
        let fields: [(Option<f64>, f64); 5] = [
            (self.reply_chain, weights.reply_chain),
            (self.topic_similarity, weights.topic_similarity),
            (self.participant_overlap, weights.participant_overlap),
            (self.temporal_proximity, weights.temporal_proximity),
            (self.mention_match, weights.mention_match),
        ];
        fields
            .into_iter()
            .filter_map(|(val, w)| val.map(|v| (v, w)))
            .collect()
    }

    /// Variant A: weighted linear combination with conditional normalization.
    ///
    /// Score = sum(signal_i * weight_i) / sum(weight_i) for present signals
    /// only. Returns `None` if no signals are present.
    pub fn score_linear(&self, weights: &ScoringWeights) -> Option<f64> {
        let pairs = self.present_signals(weights);
        if pairs.is_empty() {
            return None;
        }
        let (weighted_sum, weight_total) = pairs
            .iter()
            .fold((0.0, 0.0), |(ws, wt), &(val, w)| (ws + val * w, wt + w));
        if weight_total == 0.0 {
            None
        } else {
            Some((weighted_sum / weight_total).clamp(0.0, 1.0))
        }
    }

    /// Variant B: rule-based cascade.
    ///
    /// 1. If `reply_chain` is `Some(1.0)`, the message deterministically
    ///    belongs to this branch (score = 1.0).
    /// 2. Otherwise, combine `topic_similarity` and `temporal_proximity` as a
    ///    weighted average (of whichever are present).
    /// 3. `participant_overlap` and `mention_match` act as additive bonuses
    ///    (scaled by their weights, capped at 1.0).
    pub fn score_cascade(&self, weights: &ScoringWeights) -> Option<f64> {
        // Rule 1: deterministic reply match.
        if self.reply_chain == Some(1.0) {
            return Some(1.0);
        }

        // Rule 2: topic + temporal core.
        let core_pairs: Vec<(f64, f64)> = [
            (self.topic_similarity, weights.topic_similarity),
            (self.temporal_proximity, weights.temporal_proximity),
        ]
        .into_iter()
        .filter_map(|(val, w)| val.map(|v| (v, w)))
        .collect();

        if core_pairs.is_empty() {
            // No core signals — fall through to tiebreakers only.
            let bonus = self.tiebreaker_bonus(weights);
            return if bonus > 0.0 {
                Some(bonus.min(1.0))
            } else {
                None
            };
        }

        let (core_sum, core_weight) = core_pairs
            .iter()
            .fold((0.0, 0.0), |(s, w), &(v, wt)| (s + v * wt, w + wt));
        let base = if core_weight > 0.0 {
            core_sum / core_weight
        } else {
            0.0
        };

        // Rule 3: tiebreaker bonuses.
        let bonus = self.tiebreaker_bonus(weights);
        Some((base + bonus).clamp(0.0, 1.0))
    }

    /// Additive bonus from participant overlap and mention match, scaled by
    /// their respective weights relative to the total weight budget.
    fn tiebreaker_bonus(&self, weights: &ScoringWeights) -> f64 {
        let total = weights.total();
        if total == 0.0 {
            return 0.0;
        }
        let mut bonus = 0.0;
        if let Some(v) = self.participant_overlap {
            bonus += v * (weights.participant_overlap / total);
        }
        if let Some(v) = self.mention_match {
            bonus += v * (weights.mention_match / total);
        }
        bonus
    }
}

// ── Weights ──────────────────────────────────────────────────────────────────

/// Configurable weights for each signal in the feature vector.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoringWeights {
    pub reply_chain: f64,
    pub topic_similarity: f64,
    pub participant_overlap: f64,
    pub temporal_proximity: f64,
    pub mention_match: f64,
}

impl ScoringWeights {
    /// Sum of all weights.
    fn total(&self) -> f64 {
        self.reply_chain
            + self.topic_similarity
            + self.participant_overlap
            + self.temporal_proximity
            + self.mention_match
    }
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            reply_chain: 0.8,
            topic_similarity: 0.4,
            participant_overlap: 0.3,
            temporal_proximity: 0.3,
            mention_match: 0.2,
        }
    }
}

// ── Scoring config ───────────────────────────────────────────────────────────

/// Configuration for the branch scoring system.
#[derive(Debug, Clone)]
pub struct ScoringConfig {
    pub weights: ScoringWeights,
    /// Minimum score for a message to be assigned to an existing branch.
    pub threshold: f64,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self {
            weights: ScoringWeights::default(),
            threshold: 0.6,
        }
    }
}

// ── Score and classification ─────────────────────────────────────────────────

/// A scored candidate branch.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchScore {
    pub branch_id: BranchId,
    /// Normalized score in 0.0..=1.0.
    pub score: f64,
    /// Confidence in the assignment.
    pub confidence: AssignmentConfidence,
}

/// How confident we are about a branch assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentConfidence {
    /// Deterministic match (e.g. reply chain or thread shortcut).
    Deterministic,
    /// High confidence from multiple agreeing signals.
    High,
    /// Moderate confidence — score above threshold but not strongly.
    Moderate,
    /// Low confidence — assigned by default or with weak signals.
    Low,
}

impl fmt::Display for AssignmentConfidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Deterministic => write!(f, "deterministic"),
            Self::High => write!(f, "high"),
            Self::Moderate => write!(f, "moderate"),
            Self::Low => write!(f, "low"),
        }
    }
}

/// Result of classifying a message into a branch.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassificationResult {
    /// The message is assigned to an existing branch.
    Assigned(BranchScore),
    /// No existing branch scored above the threshold; a new branch should be
    /// created.
    NewBranch,
}

// ── Classification logic ─────────────────────────────────────────────────────

/// Classifies a message into a branch using Variant A (weighted linear).
///
/// `is_thread` short-circuits scoring for Discord threads: the thread's
/// channel ID is its branch.
///
/// `candidates` provides `(branch_id, feature_vector)` pairs for each active
/// branch.
pub fn classify_linear(
    config: &ScoringConfig,
    is_thread: bool,
    thread_branch_id: Option<BranchId>,
    candidates: &[(BranchId, FeatureVector)],
) -> ClassificationResult {
    // Thread shortcut: Discord threads skip scoring.
    if is_thread && let Some(branch_id) = thread_branch_id {
        return ClassificationResult::Assigned(BranchScore {
            branch_id,
            score: 1.0,
            confidence: AssignmentConfidence::Deterministic,
        });
    }

    let mut best: Option<BranchScore> = None;

    for (branch_id, fv) in candidates {
        if let Some(score) = fv.score_linear(&config.weights) {
            let is_better = best.as_ref().is_none_or(|b| score > b.score);
            if is_better {
                let confidence = confidence_from_score(score, config.threshold);
                best = Some(BranchScore {
                    branch_id: *branch_id,
                    score,
                    confidence,
                });
            }
        }
    }

    match best {
        Some(bs) if bs.score >= config.threshold => ClassificationResult::Assigned(bs),
        _ => ClassificationResult::NewBranch,
    }
}

/// Classifies a message into a branch using Variant B (rule-based cascade).
pub fn classify_cascade(
    config: &ScoringConfig,
    is_thread: bool,
    thread_branch_id: Option<BranchId>,
    candidates: &[(BranchId, FeatureVector)],
) -> ClassificationResult {
    // Thread shortcut: same as Variant A.
    if is_thread && let Some(branch_id) = thread_branch_id {
        return ClassificationResult::Assigned(BranchScore {
            branch_id,
            score: 1.0,
            confidence: AssignmentConfidence::Deterministic,
        });
    }

    let mut best: Option<BranchScore> = None;

    for (branch_id, fv) in candidates {
        if let Some(score) = fv.score_cascade(&config.weights) {
            let is_better = best.as_ref().is_none_or(|b| score > b.score);
            if is_better {
                // Reply chain hit in cascade is deterministic.
                let confidence = if fv.reply_chain == Some(1.0) {
                    AssignmentConfidence::Deterministic
                } else {
                    confidence_from_score(score, config.threshold)
                };
                best = Some(BranchScore {
                    branch_id: *branch_id,
                    score,
                    confidence,
                });
            }
        }
    }

    match best {
        Some(bs) if bs.score >= config.threshold => ClassificationResult::Assigned(bs),
        _ => ClassificationResult::NewBranch,
    }
}

/// Maps a raw score to a confidence level.
fn confidence_from_score(score: f64, threshold: f64) -> AssignmentConfidence {
    if score >= 0.9 {
        AssignmentConfidence::High
    } else if score >= threshold {
        AssignmentConfidence::Moderate
    } else {
        AssignmentConfidence::Low
    }
}

/// Computes temporal proximity as an exponential decay.
///
/// `elapsed_secs` is the time since the branch's most recent message.
/// `half_life_secs` controls the decay rate (default: 300 = 5 minutes).
pub fn temporal_decay(elapsed_secs: f64, half_life_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 {
        return 1.0;
    }
    if half_life_secs <= 0.0 {
        return 0.0;
    }
    (-elapsed_secs * (2.0_f64.ln()) / half_life_secs).exp()
}

/// Computes participant overlap between an incoming message's participants
/// and a branch's active participants.
///
/// Returns the fraction of `message_participants` that appear in
/// `branch_participants`. Returns 0.0 if `message_participants` is empty.
pub fn participant_overlap_ratio(message_participants: &[u64], branch_participants: &[u64]) -> f64 {
    if message_participants.is_empty() {
        return 0.0;
    }
    let matches = message_participants
        .iter()
        .filter(|p| branch_participants.contains(p))
        .count();
    matches as f64 / message_participants.len() as f64
}

// ── Bridge from BranchProfile (Lain's scorer) ──────────────────────────────

/// Builds a [`FeatureVector`] from Lain's `BranchProfile`-style data.
///
/// This is the bridge layer: converts the scorer's domain types into the
/// feature vector representation that `classify_linear` / `classify_cascade`
/// consume.
pub fn build_feature_vector(
    reply_to_message_in_branch: bool,
    topic_similarity: Option<f64>,
    message_participants: &[u64],
    branch_participants: &[u64],
    elapsed_secs: f64,
    half_life_secs: f64,
    has_mention_match: bool,
) -> FeatureVector {
    let reply_chain = if reply_to_message_in_branch {
        Some(1.0)
    } else {
        Some(0.0)
    };

    let participant_overlap = if message_participants.is_empty() {
        None
    } else {
        Some(participant_overlap_ratio(
            message_participants,
            branch_participants,
        ))
    };

    let temporal_proximity = Some(temporal_decay(elapsed_secs, half_life_secs));

    let mention_match = if has_mention_match {
        Some(1.0)
    } else {
        Some(0.0)
    };

    FeatureVector {
        reply_chain,
        topic_similarity,
        participant_overlap,
        temporal_proximity,
        mention_match,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn bid(raw: u64) -> BranchId {
        BranchId::new_for_test(raw)
    }

    // ── Mock embedder ────────────────────────────────────────────────────

    struct MockEmbedder;

    impl Embedder for MockEmbedder {
        fn embed(&self, text: &str) -> Vec<f32> {
            let c = text.chars().next().unwrap_or('a') as u32 as f32;
            let raw = [c, c * 0.5, c * 0.25];
            let mag = (raw[0] * raw[0] + raw[1] * raw[1] + raw[2] * raw[2]).sqrt();
            if mag == 0.0 {
                vec![0.0, 0.0, 0.0]
            } else {
                vec![raw[0] / mag, raw[1] / mag, raw[2] / mag]
            }
        }
    }

    // ── Cosine similarity ────────────────────────────────────────────────

    #[test]
    fn cosine_similarity_identical_vectors() {
        let e = MockEmbedder;
        let v = e.embed("hello");
        let sim = e.cosine_similarity(&v, &v);
        assert!(
            (sim - 1.0).abs() < 1e-9,
            "identical vectors should have similarity 1.0, got {sim}"
        );
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let e = MockEmbedder;
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        let sim = e.cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-9);
    }

    #[test]
    fn cosine_similarity_zero_vector() {
        let e = MockEmbedder;
        let a = vec![1.0_f32, 2.0, 3.0];
        let b = vec![0.0_f32, 0.0, 0.0];
        let sim = e.cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-9);
    }

    // ── Temporal decay ──────────────────────────────────────────────────

    #[test]
    fn temporal_decay_zero_elapsed() {
        assert_eq!(temporal_decay(0.0, 300.0), 1.0);
    }

    #[test]
    fn temporal_decay_at_half_life() {
        let val = temporal_decay(300.0, 300.0);
        assert!((val - 0.5).abs() < 1e-9);
    }

    #[test]
    fn temporal_decay_large_elapsed() {
        let val = temporal_decay(3000.0, 300.0);
        assert!(val < 0.01);
    }

    #[test]
    fn temporal_decay_negative_elapsed() {
        assert_eq!(temporal_decay(-10.0, 300.0), 1.0);
    }

    #[test]
    fn temporal_decay_zero_half_life() {
        assert_eq!(temporal_decay(10.0, 0.0), 0.0);
    }

    // ── Participant overlap ─────────────────────────────────────────────

    #[test]
    fn participant_overlap_full() {
        let ratio = participant_overlap_ratio(&[1, 2, 3], &[1, 2, 3, 4]);
        assert!((ratio - 1.0).abs() < 1e-9);
    }

    #[test]
    fn participant_overlap_partial() {
        let ratio = participant_overlap_ratio(&[1, 2, 3], &[2, 4, 5]);
        let expected = 1.0 / 3.0;
        assert!((ratio - expected).abs() < 1e-9);
    }

    #[test]
    fn participant_overlap_none() {
        let ratio = participant_overlap_ratio(&[1, 2], &[3, 4]);
        assert!(ratio.abs() < 1e-9);
    }

    #[test]
    fn participant_overlap_empty_message() {
        let ratio = participant_overlap_ratio(&[], &[1, 2]);
        assert_eq!(ratio, 0.0);
    }

    // ── Score: Variant A (linear) ───────────────────────────────────────

    #[test]
    fn linear_score_all_signals_present() {
        let fv = FeatureVector {
            reply_chain: Some(1.0),
            topic_similarity: Some(0.8),
            participant_overlap: Some(0.5),
            temporal_proximity: Some(0.7),
            mention_match: Some(1.0),
        };
        let weights = ScoringWeights::default();
        let score = fv.score_linear(&weights).expect("should produce a score");
        assert!((score - 0.84).abs() < 1e-9, "expected 0.84, got {score}");
    }

    #[test]
    fn linear_score_reply_only() {
        let fv = FeatureVector {
            reply_chain: Some(1.0),
            topic_similarity: None,
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        let score = fv
            .score_linear(&ScoringWeights::default())
            .expect("reply-only should score");
        assert!((score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn linear_score_no_signals() {
        let fv = FeatureVector {
            reply_chain: None,
            topic_similarity: None,
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        assert!(fv.score_linear(&ScoringWeights::default()).is_none());
    }

    // ── Score: Variant B (cascade) ──────────────────────────────────────

    #[test]
    fn cascade_reply_deterministic() {
        let fv = FeatureVector {
            reply_chain: Some(1.0),
            topic_similarity: Some(0.1),
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        let score = fv
            .score_cascade(&ScoringWeights::default())
            .expect("reply hit should score");
        assert!((score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cascade_topic_and_temporal() {
        let fv = FeatureVector {
            reply_chain: None,
            topic_similarity: Some(0.8),
            participant_overlap: None,
            temporal_proximity: Some(0.6),
            mention_match: None,
        };
        let w = ScoringWeights::default();
        let score = fv.score_cascade(&w).expect("should produce a score");
        let expected = (0.8 * 0.4 + 0.6 * 0.3) / (0.4 + 0.3);
        assert!((score - expected).abs() < 1e-9);
    }

    #[test]
    fn cascade_no_signals() {
        let fv = FeatureVector {
            reply_chain: None,
            topic_similarity: None,
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        assert!(fv.score_cascade(&ScoringWeights::default()).is_none());
    }

    // ── Classification ──────────────────────────────────────────────────

    #[test]
    fn thread_shortcut_linear() {
        let config = ScoringConfig::default();
        let result = classify_linear(&config, true, Some(bid(42)), &[]);
        assert_eq!(
            result,
            ClassificationResult::Assigned(BranchScore {
                branch_id: bid(42),
                score: 1.0,
                confidence: AssignmentConfidence::Deterministic,
            })
        );
    }

    #[test]
    fn thread_shortcut_cascade() {
        let config = ScoringConfig::default();
        let result = classify_cascade(&config, true, Some(bid(42)), &[]);
        assert_eq!(
            result,
            ClassificationResult::Assigned(BranchScore {
                branch_id: bid(42),
                score: 1.0,
                confidence: AssignmentConfidence::Deterministic,
            })
        );
    }

    #[test]
    fn new_branch_when_no_candidates() {
        let config = ScoringConfig::default();
        assert_eq!(
            classify_linear(&config, false, None, &[]),
            ClassificationResult::NewBranch,
        );
        assert_eq!(
            classify_cascade(&config, false, None, &[]),
            ClassificationResult::NewBranch,
        );
    }

    #[test]
    fn new_branch_when_below_threshold() {
        let config = ScoringConfig::default();
        let fv = FeatureVector {
            reply_chain: None,
            topic_similarity: Some(0.2),
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        let candidates = vec![(bid(1), fv)];
        assert_eq!(
            classify_linear(&config, false, None, &candidates),
            ClassificationResult::NewBranch,
        );
    }

    #[test]
    fn reply_to_assigns_to_correct_branch() {
        let config = ScoringConfig::default();
        let fv_match = FeatureVector {
            reply_chain: Some(1.0),
            topic_similarity: None,
            participant_overlap: None,
            temporal_proximity: None,
            mention_match: None,
        };
        let fv_other = FeatureVector {
            reply_chain: Some(0.0),
            topic_similarity: Some(0.5),
            participant_overlap: None,
            temporal_proximity: Some(0.5),
            mention_match: None,
        };
        let candidates = vec![(bid(10), fv_other), (bid(20), fv_match)];

        let result = classify_linear(&config, false, None, &candidates);
        match result {
            ClassificationResult::Assigned(bs) => {
                assert_eq!(bs.branch_id, bid(20));
            }
            _ => panic!("expected Assigned"),
        }

        let result = classify_cascade(&config, false, None, &candidates);
        match result {
            ClassificationResult::Assigned(bs) => {
                assert_eq!(bs.branch_id, bid(20));
                assert_eq!(bs.confidence, AssignmentConfidence::Deterministic);
            }
            _ => panic!("expected Assigned"),
        }
    }

    #[test]
    fn selects_highest_scoring_branch() {
        let config = ScoringConfig::default();
        let fv_low = FeatureVector {
            reply_chain: None,
            topic_similarity: Some(0.6),
            participant_overlap: None,
            temporal_proximity: Some(0.5),
            mention_match: None,
        };
        let fv_high = FeatureVector {
            reply_chain: None,
            topic_similarity: Some(0.95),
            participant_overlap: Some(0.8),
            temporal_proximity: Some(0.9),
            mention_match: None,
        };
        let candidates = vec![(bid(1), fv_low), (bid(2), fv_high)];

        let result = classify_linear(&config, false, None, &candidates);
        match result {
            ClassificationResult::Assigned(bs) => {
                assert_eq!(bs.branch_id, bid(2));
            }
            _ => panic!("expected Assigned"),
        }
    }

    #[test]
    fn variants_agree_on_strong_reply() {
        let config = ScoringConfig::default();
        let fv = FeatureVector {
            reply_chain: Some(1.0),
            topic_similarity: Some(0.9),
            participant_overlap: Some(1.0),
            temporal_proximity: Some(0.95),
            mention_match: Some(1.0),
        };
        let candidates = vec![(bid(1), fv)];

        let linear = classify_linear(&config, false, None, &candidates);
        let cascade = classify_cascade(&config, false, None, &candidates);

        match (&linear, &cascade) {
            (ClassificationResult::Assigned(a), ClassificationResult::Assigned(b)) => {
                assert_eq!(a.branch_id, b.branch_id);
            }
            _ => panic!("expected both Assigned"),
        }
    }

    #[test]
    fn variants_agree_on_clear_new_branch() {
        let config = ScoringConfig::default();
        let fv = FeatureVector {
            reply_chain: None,
            topic_similarity: Some(0.1),
            participant_overlap: Some(0.0),
            temporal_proximity: Some(0.05),
            mention_match: None,
        };
        let candidates = vec![(bid(1), fv)];

        assert_eq!(
            classify_linear(&config, false, None, &candidates),
            ClassificationResult::NewBranch,
        );
        assert_eq!(
            classify_cascade(&config, false, None, &candidates),
            ClassificationResult::NewBranch,
        );
    }

    // ── Confidence mapping ──────────────────────────────────────────────

    #[test]
    fn confidence_levels() {
        assert_eq!(confidence_from_score(0.95, 0.6), AssignmentConfidence::High);
        assert_eq!(
            confidence_from_score(0.75, 0.6),
            AssignmentConfidence::Moderate
        );
        assert_eq!(confidence_from_score(0.4, 0.6), AssignmentConfidence::Low);
    }

    #[test]
    fn confidence_display() {
        assert_eq!(
            format!("{}", AssignmentConfidence::Deterministic),
            "deterministic"
        );
        assert_eq!(format!("{}", AssignmentConfidence::High), "high");
        assert_eq!(format!("{}", AssignmentConfidence::Moderate), "moderate");
        assert_eq!(format!("{}", AssignmentConfidence::Low), "low");
    }

    // ── build_feature_vector bridge ─────────────────────────────────────

    #[test]
    fn build_feature_vector_reply_match() {
        let fv = build_feature_vector(true, Some(0.8), &[1, 2], &[2, 3], 10.0, 300.0, true);
        assert_eq!(fv.reply_chain, Some(1.0));
        assert_eq!(fv.topic_similarity, Some(0.8));
        assert!(fv.participant_overlap.is_some());
        assert!(fv.temporal_proximity.is_some());
        assert_eq!(fv.mention_match, Some(1.0));
    }

    #[test]
    fn build_feature_vector_no_participants() {
        let fv = build_feature_vector(false, None, &[], &[1, 2], 0.0, 300.0, false);
        assert_eq!(fv.reply_chain, Some(0.0));
        assert_eq!(fv.topic_similarity, None);
        assert!(fv.participant_overlap.is_none());
        assert_eq!(fv.mention_match, Some(0.0));
    }
}
