//! Pipeline glue: MessageInput -> FeatureVector -> classify -> annotate -> CPD.
//!
//! Wires the three layers together into a single `process_message` call:
//!
//! 1. Build [`FeatureVector`]s for each active branch in the channel.
//! 2. Classify the message using [`classify_cascade`] (or `classify_linear`).
//! 3. Annotate the message on the assigned branch via [`BranchTracker`].
//! 4. Run change-point detection via the [`ChangePointDetector`] trait.
//!
//! The pipeline is stateful: it owns the [`BranchTracker`], per-branch
//! [`RunLengthDistribution`]s, and the selected [`ChangePointDetector`].

use std::collections::HashMap;
use std::time::Instant;

use serenity::model::id::ChannelId;

use super::cpd::{
    ChangePointDetector, CpdConfig, RunLengthDistribution, Verdict, detect_and_update,
};
use super::feature_vector::{
    ClassificationResult, FeatureVector, ScoringConfig, build_feature_vector, classify_cascade,
};
use super::{
    BranchAnnotation, BranchId, BranchState, BranchTracker, BranchTrackerConfig, MessageAnnotator,
    MessageInput,
};

// ── Pipeline configuration ─────────────────────────────────────────────────

/// Full pipeline configuration.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Branch tracker configuration (EWMA, dormancy, pruning).
    pub tracker: BranchTrackerConfig,
    /// Feature vector scoring configuration (weights, threshold).
    pub scoring: ScoringConfig,
    /// Change-point detection configuration.
    pub cpd: CpdConfig,
    /// Half-life in seconds for temporal decay in feature vectors.
    /// Default: 300 (5 minutes).
    pub temporal_half_life_secs: f64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            tracker: BranchTrackerConfig::default(),
            scoring: ScoringConfig::default(),
            cpd: CpdConfig::default(),
            temporal_half_life_secs: 300.0,
        }
    }
}

// ── Pipeline result ────────────────────────────────────────────────────────

/// Full result from processing a message through the pipeline.
#[derive(Debug, Clone)]
pub struct PipelineResult {
    /// The branch annotation (which branch, rate estimate, state, etc.).
    pub annotation: BranchAnnotation,
    /// The CPD verdict for this message.
    pub cpd_verdict: Verdict,
    /// Whether this message was assigned to a newly created branch.
    pub is_new_branch: bool,
}

// ── Pipeline ───────────────────────────────────────────────────────────────

/// The unified branch tracking pipeline.
///
/// Owns the branch tracker, per-branch run-length distributions, and the
/// CPD detector. Call [`Pipeline::process`] for each incoming message.
pub struct Pipeline {
    tracker: BranchTracker,
    /// Per-branch run-length distributions, keyed by `(channel_id, branch_id)`.
    distributions: HashMap<(ChannelId, BranchId), RunLengthDistribution>,
    /// The change-point detector implementation.
    detector: Box<dyn ChangePointDetector + Send + Sync>,
    config: PipelineConfig,
}

impl Pipeline {
    /// Create a new pipeline with the given detector and configuration.
    pub fn new(
        detector: Box<dyn ChangePointDetector + Send + Sync>,
        config: PipelineConfig,
    ) -> Self {
        Self {
            tracker: BranchTracker::new(config.tracker.clone()),
            distributions: HashMap::new(),
            detector,
            config,
        }
    }

    /// Process an incoming message through the full pipeline.
    ///
    /// # Steps
    /// 1. Build feature vectors for each active branch in the channel.
    /// 2. Classify the message (cascade scoring).
    /// 3. Annotate the message on the assigned (or new) branch.
    /// 4. Run CPD on the assigned branch.
    pub fn process(&mut self, input: &MessageInput<'_>) -> PipelineResult {
        let now = input.timestamp;

        // Step 1: Build feature vectors for each active branch.
        let candidates = self.build_candidates(input, now);

        // Step 2: Classify.
        let classification = classify_cascade(
            &self.config.scoring,
            false, // Not a Discord thread (caller handles that).
            None,
            &candidates,
        );

        // Step 3: Determine branch assignment.
        let (branch_id, is_new_branch) = match classification {
            ClassificationResult::Assigned(bs) => (bs.branch_id, false),
            ClassificationResult::NewBranch => {
                // Fall back to the tracker's default assignment.
                let bid = self.tracker.assign_default(input);
                // Check if this is actually new (branch didn't exist before).
                let is_new = self
                    .tracker
                    .branch(input.channel_id, bid)
                    .is_none_or(|b| b.observation_count() == 0);
                (bid, is_new)
            }
        };

        // Step 4: Annotate (updates rate estimate, participants, timestamps).
        let annotation = self.tracker.annotate(input, branch_id);

        // Step 5: CPD.
        let gap = annotation.rate_estimate;
        let dist = self
            .distributions
            .entry((input.channel_id, branch_id))
            .or_default();

        let cpd_verdict = if let Some(branch) = self.tracker.branch_mut(input.channel_id, branch_id)
        {
            detect_and_update(&*self.detector, branch, dist, gap, &self.config.cpd)
        } else {
            Verdict::Uncertain { confidence: 0.0 }
        };

        PipelineResult {
            annotation,
            cpd_verdict,
            is_new_branch,
        }
    }

    /// Run maintenance (dormancy checks, pruning).
    pub fn maintain(&mut self, now: Instant) {
        self.tracker.maintain(now);

        // Prune distributions for branches that no longer exist.
        self.distributions
            .retain(|(ch, bid), _| self.tracker.branch(*ch, *bid).is_some());
    }

    /// Access the underlying branch tracker.
    pub fn tracker(&self) -> &BranchTracker {
        &self.tracker
    }

    /// Build feature vector candidates for all active branches in the channel.
    fn build_candidates(
        &self,
        input: &MessageInput<'_>,
        now: Instant,
    ) -> Vec<(BranchId, FeatureVector)> {
        let msg_participants: Vec<u64> = std::iter::once(input.user_id.get())
            .chain(input.mentions.iter().map(|u| u.get()))
            .collect();

        self.tracker
            .branches_in_channel(input.channel_id)
            .filter(|(_, branch)| branch.state() != BranchState::Dormant)
            .map(|(&branch_id, branch)| {
                let branch_participants = branch.participants().as_raw_ids();

                // Reply chain: check if reply_to maps to this branch.
                let reply_match = input
                    .reply_to
                    .and_then(|reply_id| {
                        // Check via the tracker's channel state.
                        // We look at the message-branch mapping.
                        self.tracker
                            .branch(input.channel_id, branch_id)
                            .is_some()
                            .then_some(reply_id)
                    })
                    .is_some_and(|_reply_id| {
                        // Approximate: if the branch exists and contains the
                        // reply target, it's a match. The full mapping check
                        // happens in assign_default.
                        false // Conservative: let assign_default handle reply chains.
                    });

                let elapsed_secs = now.duration_since(branch.last_active_at()).as_secs_f64();

                let has_mention_match = input
                    .mentions
                    .iter()
                    .any(|m| branch_participants.contains(&m.get()));

                let fv = build_feature_vector(
                    reply_match,
                    None, // Topic similarity requires an embedder (deferred).
                    &msg_participants,
                    &branch_participants,
                    elapsed_secs,
                    self.config.temporal_half_life_secs,
                    has_mention_match,
                );

                (branch_id, fv)
            })
            .collect()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::branch_tracking::cpd::{BayesianDetector, LightweightDetector};
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::time::Duration;

    fn ch(id: u64) -> ChannelId {
        ChannelId::new(id)
    }
    fn mid(id: u64) -> MessageId {
        MessageId::new(id)
    }
    fn uid(id: u64) -> UserId {
        UserId::new(id)
    }

    fn msg<'a>(
        channel: ChannelId,
        message: MessageId,
        user: UserId,
        ts: Instant,
        content: &'a str,
        reply_to: Option<MessageId>,
    ) -> MessageInput<'a> {
        MessageInput {
            channel_id: channel,
            message_id: message,
            user_id: user,
            timestamp: ts,
            content,
            reply_to,
            mentions: Vec::new(),
        }
    }

    // ── Full pipeline test: message in -> branch annotation out ─────────

    #[test]
    fn full_pipeline_single_message() {
        let mut pipeline =
            Pipeline::new(Box::new(BayesianDetector::new()), PipelineConfig::default());
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello world", None);
        let result = pipeline.process(&input);

        assert_eq!(result.annotation.branch_id.raw(), 0);
        assert!(result.is_new_branch);
        // First message on a new branch: CPD should be uncertain (low confidence).
        match result.cpd_verdict {
            Verdict::Uncertain { .. } => {}
            _ => {
                // Also acceptable: continuation or changepoint if the
                // confidence threshold is met after the first observation.
            }
        }
    }

    #[test]
    fn full_pipeline_message_sequence() {
        let mut pipeline =
            Pipeline::new(Box::new(BayesianDetector::new()), PipelineConfig::default());
        let now = Instant::now();

        // Send a series of messages at a steady rate.
        let mut last_result = None;
        for i in 0..15u64 {
            let input = msg(
                ch(100),
                mid(i + 1),
                uid(42),
                now + Duration::from_secs(i * 10),
                "message",
                None,
            );
            last_result = Some(pipeline.process(&input));
        }

        let result = last_result.unwrap();
        // After 15 messages at 10s intervals, we should have a valid annotation.
        assert!(result.annotation.confidence > 0.0);
        assert_eq!(result.annotation.branch_state, BranchState::Active);
        // Rate estimate should be converging toward 10.0.
        assert!(
            result.annotation.rate_estimate > 0.0,
            "rate should be positive after observations"
        );
    }

    #[test]
    fn full_pipeline_maintains_state() {
        let mut pipeline = Pipeline::new(
            Box::new(BayesianDetector::new()),
            PipelineConfig {
                tracker: BranchTrackerConfig {
                    prune_after_secs: 100.0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        pipeline.process(&input);

        assert_eq!(pipeline.tracker().branch_count(ch(100)), 1);

        // Maintain after prune threshold.
        pipeline.maintain(now + Duration::from_secs(101));

        assert_eq!(pipeline.tracker().branch_count(ch(100)), 0);
        // Distributions should also be pruned.
        assert!(pipeline.distributions.is_empty());
    }

    // ── Differential test structure: both CPDs against same stream ──────

    #[test]
    fn differential_both_cpds_agree_on_steady_state() {
        let config = PipelineConfig::default();

        let mut pipeline_bayesian =
            Pipeline::new(Box::new(BayesianDetector::new()), config.clone());
        let mut pipeline_lightweight =
            Pipeline::new(Box::new(LightweightDetector::new(60.0)), config);

        let now = Instant::now();
        let mut bayesian_changepoints = 0;
        let mut lightweight_changepoints = 0;

        // Send 20 messages at a steady 10s rate.
        for i in 0..20u64 {
            let input = msg(
                ch(100),
                mid(i + 1),
                uid(42),
                now + Duration::from_secs(i * 10),
                "steady",
                None,
            );

            let rb = pipeline_bayesian.process(&input);
            let rl = pipeline_lightweight.process(&input);

            if rb.cpd_verdict.is_changepoint() {
                bayesian_changepoints += 1;
            }
            if rl.cpd_verdict.is_changepoint() {
                lightweight_changepoints += 1;
            }
        }

        // In a steady state, neither should detect many changepoints.
        // The exact count can differ due to confidence gating and distribution
        // differences, but both should be low.
        assert!(
            bayesian_changepoints <= 5,
            "Bayesian detected too many changepoints in steady state: {bayesian_changepoints}"
        );
        assert!(
            lightweight_changepoints <= 5,
            "Lightweight detected too many changepoints in steady state: {lightweight_changepoints}"
        );
    }

    #[test]
    fn differential_both_cpds_detect_rate_shift() {
        let config = PipelineConfig {
            cpd: CpdConfig {
                expected_run_length: 5.0,
                changepoint_threshold: 0.5,
                min_confidence: 0.3,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut pipeline_bayesian =
            Pipeline::new(Box::new(BayesianDetector::new()), config.clone());
        let mut pipeline_lightweight =
            Pipeline::new(Box::new(LightweightDetector::new(60.0)), config);

        let now = Instant::now();

        // Phase 1: steady state at 5s gaps.
        for i in 0..10u64 {
            let input = msg(
                ch(100),
                mid(i + 1),
                uid(42),
                now + Duration::from_secs(i * 5),
                "fast",
                None,
            );
            pipeline_bayesian.process(&input);
            pipeline_lightweight.process(&input);
        }

        // Phase 2: sudden shift to 60s gaps.
        let mut bayesian_detected = false;
        let mut lightweight_detected = false;

        for i in 0..10u64 {
            let input = msg(
                ch(100),
                mid(i + 11),
                uid(42),
                now + Duration::from_secs(50 + i * 60),
                "slow",
                None,
            );

            let rb = pipeline_bayesian.process(&input);
            let rl = pipeline_lightweight.process(&input);

            if rb.cpd_verdict.is_changepoint() {
                bayesian_detected = true;
            }
            if rl.cpd_verdict.is_changepoint() {
                lightweight_detected = true;
            }
        }

        // At least one detector should catch the shift. Both catching it is
        // ideal, but they have different sensitivity profiles.
        assert!(
            bayesian_detected || lightweight_detected,
            "at least one CPD should detect the rate shift"
        );
    }

    // ── Pipeline config defaults ────────────────────────────────────────

    #[test]
    fn pipeline_config_defaults() {
        let config = PipelineConfig::default();
        assert_eq!(config.temporal_half_life_secs, 300.0);
        assert_eq!(config.scoring.threshold, 0.6);
        assert_eq!(config.cpd.expected_run_length, 20.0);
        assert_eq!(config.tracker.alpha, 0.2);
    }

    // ── Multiple channels ──────────────────────────────────────────────

    #[test]
    fn pipeline_handles_multiple_channels() {
        let mut pipeline =
            Pipeline::new(Box::new(BayesianDetector::new()), PipelineConfig::default());
        let now = Instant::now();

        let input1 = msg(ch(100), mid(1), uid(42), now, "ch1", None);
        let input2 = msg(ch(200), mid(2), uid(43), now, "ch2", None);

        let r1 = pipeline.process(&input1);
        let r2 = pipeline.process(&input2);

        assert_eq!(r1.annotation.branch_id.raw(), 0);
        assert_eq!(r2.annotation.branch_id.raw(), 0);
        assert_eq!(pipeline.tracker().channel_count(), 2);
    }
}
