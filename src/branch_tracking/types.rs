//! Core types for conversation branch tracking.

use std::time::Instant;

use serenity::model::id::{ChannelId, MessageId, UserId};

// ── Newtypes ────────────────────────────────────────────────────────────────

/// Opaque branch identifier, unique within a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BranchId(u64);

impl BranchId {
    /// Creates a new `BranchId` from a raw counter value.
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// Creates a `BranchId` for use in tests. Not exposed in the public API.
    #[cfg(test)]
    pub(crate) fn new_for_test(raw: u64) -> Self {
        Self(raw)
    }

    /// Returns the raw numeric value (for serialization/logging only).
    pub fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for BranchId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "branch/{}", self.0)
    }
}

// ── Branch state ────────────────────────────────────────────────────────────

/// Lifecycle state of a conversation branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// Fewer than 2 observations; rate estimate is bootstrapped.
    New,
    /// Actively receiving messages within the expected cadence.
    Active,
    /// No messages for `dormancy_multiplier * rate_estimate` seconds.
    Dormant,
}

// ── Rate estimator trait ────────────────────────────────────────────────────

/// Abstraction over rate estimation strategy.
///
/// The CPD layer and feature vector layer consume this trait without knowing
/// whether the implementation is EWMA, windowed median, or something else.
pub trait RateEstimator {
    /// Current estimated inter-message gap in seconds.
    fn estimate(&self) -> f64;

    /// Confidence in the estimate: `[0.0, 1.0]`.
    fn confidence(&self) -> f32;

    /// Number of gap observations recorded.
    fn observation_count(&self) -> u32;

    /// Record a new observed inter-message gap (seconds).
    fn observe(&mut self, gap_secs: f64);

    /// Record a new observed gap with a boosted alpha (for reactivation).
    fn observe_boosted(&mut self, gap_secs: f64, alpha_boost: f64);
}

// ── Participant set ─────────────────────────────────────────────────────────

/// Compact participant tracking — stores user IDs seen on this branch.
#[derive(Debug, Clone, Default)]
pub struct ParticipantSet {
    users: Vec<UserId>,
}

impl ParticipantSet {
    pub(crate) fn new() -> Self {
        Self { users: Vec::new() }
    }

    /// Adds a participant if not already present. O(n) but n is small (< 20).
    pub(crate) fn insert(&mut self, user_id: UserId) {
        if !self.users.contains(&user_id) {
            self.users.push(user_id);
        }
    }

    /// Returns the participant slice for Jaccard overlap computation.
    pub fn as_slice(&self) -> &[UserId] {
        &self.users
    }

    /// Returns raw u64 IDs for feature vector compatibility.
    pub fn as_raw_ids(&self) -> Vec<u64> {
        self.users.iter().map(|u| u.get()).collect()
    }

    /// Number of unique participants.
    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }
}

// ── Branch ──────────────────────────────────────────────────────────────────

/// A single conversation branch within a channel.
///
/// Owns its rate estimate and exposes `increment_run_length` /
/// `reset_run_length` for the Bayesian CPD layer to call.
pub struct Branch {
    pub(crate) id: BranchId,
    pub(crate) rate: Box<dyn RateEstimator + Send + Sync>,
    pub(crate) state: BranchState,
    pub(crate) run_length: u32,
    pub(crate) participants: ParticipantSet,
    pub(crate) created_at: Instant,
    pub(crate) last_active_at: Instant,
}

impl Branch {
    /// Creates a new branch with the given rate estimator.
    pub(crate) fn new(
        id: BranchId,
        rate: Box<dyn RateEstimator + Send + Sync>,
        now: Instant,
    ) -> Self {
        Self {
            id,
            rate,
            state: BranchState::New,
            run_length: 0,
            participants: ParticipantSet::new(),
            created_at: now,
            last_active_at: now,
        }
    }

    /// Branch identifier.
    pub fn id(&self) -> BranchId {
        self.id
    }

    /// Current lifecycle state.
    pub fn state(&self) -> BranchState {
        self.state
    }

    /// Current rate estimate (inter-message gap in seconds).
    pub fn rate_estimate(&self) -> f64 {
        self.rate.estimate()
    }

    /// Confidence in the rate estimate: `[0.0, 1.0]`.
    pub fn rate_confidence(&self) -> f32 {
        self.rate.confidence()
    }

    /// Number of gap observations.
    pub fn observation_count(&self) -> u32 {
        self.rate.observation_count()
    }

    /// Current run length (messages since last change-point).
    pub fn run_length(&self) -> u32 {
        self.run_length
    }

    /// Participant set for this branch.
    pub fn participants(&self) -> &ParticipantSet {
        &self.participants
    }

    /// When this branch was created.
    pub fn created_at(&self) -> Instant {
        self.created_at
    }

    /// When this branch last received a message.
    pub fn last_active_at(&self) -> Instant {
        self.last_active_at
    }

    /// Increment run length. Called by the CPD layer when the current run
    /// continues (no change-point detected).
    pub fn increment_run_length(&mut self) {
        self.run_length = self.run_length.saturating_add(1);
    }

    /// Reset run length to zero. Called by the CPD layer when a change-point
    /// is detected.
    pub fn reset_run_length(&mut self) {
        self.run_length = 0;
    }

    /// Record a message on this branch, updating rate estimate and timestamps.
    pub(crate) fn record_message(
        &mut self,
        user_id: UserId,
        now: Instant,
        dormancy_multiplier: f64,
        reactivation_alpha_boost: f64,
    ) {
        let gap_secs = now.duration_since(self.last_active_at).as_secs_f64();

        match self.state {
            BranchState::Dormant => {
                // Reactivation: boosted alpha for faster convergence.
                self.rate
                    .observe_boosted(gap_secs, reactivation_alpha_boost);
                self.state = BranchState::Active;
            }
            BranchState::New => {
                self.rate.observe(gap_secs);
                // Transition to Active after first real observation.
                if self.rate.observation_count() >= 1 {
                    self.state = BranchState::Active;
                }
            }
            BranchState::Active => {
                self.rate.observe(gap_secs);
            }
        }

        self.participants.insert(user_id);
        self.last_active_at = now;

        // Re-check dormancy (shouldn't trigger right after a message, but
        // keeps the state machine consistent).
        self.update_dormancy(now, dormancy_multiplier);
    }

    /// Check and update dormancy state based on elapsed time.
    ///
    /// Only transitions Active -> Dormant. Dormant -> Active transitions
    /// happen exclusively in `record_message` when a new message arrives.
    pub(crate) fn update_dormancy(&mut self, now: Instant, dormancy_multiplier: f64) {
        if self.state != BranchState::Active {
            return; // Only Active branches can go Dormant via time elapsed.
        }
        let elapsed = now.duration_since(self.last_active_at).as_secs_f64();
        let threshold = self.rate.estimate() * dormancy_multiplier;
        if elapsed > threshold {
            self.state = BranchState::Dormant;
        }
    }
}

// ── Branch annotation ───────────────────────────────────────────────────────

/// Annotation produced by the branch tracker for each incoming message.
///
/// Downstream layers (delivery buffer, CPD, feature vector) consume this
/// without coupling to the tracker internals.
#[derive(Debug, Clone)]
pub struct BranchAnnotation {
    /// Which branch this message was assigned to.
    pub branch_id: BranchId,
    /// Lifecycle state of the branch at the time of annotation.
    pub branch_state: BranchState,
    /// Current EWMA (or median) rate estimate in seconds.
    pub rate_estimate: f64,
    /// Confidence in the rate estimate: `[0.0, 1.0]`.
    pub confidence: f32,
    /// Messages since last change-point on this branch.
    pub run_length: u32,
}

// ── Message input ───────────────────────────────────────────────────────────

/// Input to the branch tracker: a parsed Discord message with just the fields
/// needed for branch tracking.
pub struct MessageInput<'a> {
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub timestamp: Instant,
    /// Content for topic embedding.
    pub content: &'a str,
    /// If the message is a reply, the ID of the replied-to message.
    pub reply_to: Option<MessageId>,
    /// User IDs mentioned in the message.
    pub mentions: Vec<UserId>,
}

// ── Message annotator trait ─────────────────────────────────────────────────

/// Pipeline integration trait. The branch tracker implements this so the
/// dione event handler can call it without knowing the internals.
pub trait MessageAnnotator {
    /// Process an incoming message and produce a branch annotation.
    ///
    /// The branch ID in the annotation is determined by the feature vector
    /// layer (or a default assignment strategy). This method updates rate
    /// estimates and branch state for the assigned branch.
    fn annotate(&mut self, input: &MessageInput<'_>, branch_id: BranchId) -> BranchAnnotation;

    /// Assign a message to a branch using a simple heuristic (for use when
    /// the feature vector layer is not yet integrated). Returns the branch ID.
    ///
    /// Default strategy: if the message is a reply to a message on a known
    /// branch, assign to that branch. Otherwise, assign to the most recently
    /// active branch in the channel, or create a new one.
    fn assign_default(&mut self, input: &MessageInput<'_>) -> BranchId;

    /// Run dormancy checks and prune expired branches. Call periodically
    /// (e.g. every 60s) or before annotation.
    fn maintain(&mut self, now: Instant);
}

// ── Configuration ───────────────────────────────────────────────────────────

/// Configuration for the branch tracker.
#[derive(Debug, Clone)]
pub struct BranchTrackerConfig {
    /// EWMA smoothing factor. Default: 0.2.
    pub alpha: f64,
    /// Initial rate estimate when no observations exist (seconds). Default: 60.
    pub initial_rate_estimate_secs: f64,
    /// Dormancy threshold: `dormancy_multiplier * rate_estimate`. Default: 10.
    pub dormancy_multiplier: f64,
    /// Prune branches inactive for this many seconds. Default: 3600.
    pub prune_after_secs: f64,
    /// Maximum branches per channel. Default: 50.
    pub max_branches_per_channel: usize,
    /// Alpha boost factor on reactivation. Default: 1.5.
    pub reactivation_alpha_boost: f64,
}

impl Default for BranchTrackerConfig {
    fn default() -> Self {
        Self {
            alpha: 0.2,
            initial_rate_estimate_secs: 60.0,
            dormancy_multiplier: 10.0,
            prune_after_secs: 3600.0,
            max_branches_per_channel: 50,
            reactivation_alpha_boost: 1.5,
        }
    }
}
