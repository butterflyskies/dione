//! Conversation branch tracking for Discord channels.
//!
//! Tracks per-branch message rate using an EWMA estimator, managing branch
//! lifecycle (creation, activity, dormancy, pruning) and producing
//! [`BranchAnnotation`]s that downstream layers (Bayesian CPD, feature vector)
//! can consume without knowing the rate estimation internals.
//!
//! # Pipeline position
//!
//! ```text
//! Discord gateway -> message parsing -> ** branch tracker ** -> delivery buffer -> construct
//! ```

pub mod cpd;
pub mod ewma;

use std::collections::HashMap;
use std::time::Instant;

use serenity::model::id::{ChannelId, MessageId, UserId};

// ── Newtypes ────────────────────────────────────────────────────────────────

/// Opaque branch identifier, unique within a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BranchId(u64);

impl BranchId {
    /// Creates a new `BranchId` from a raw counter value.
    fn new(raw: u64) -> Self {
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

    /// Confidence in the estimate: `min(1.0, observations / 10.0)`.
    fn confidence(&self) -> f32;

    /// Number of gap observations recorded.
    fn observation_count(&self) -> u32;

    /// Record a new observed inter-message gap (seconds).
    fn observe(&mut self, gap_secs: f64);

    /// Record a new observed gap with a boosted alpha (for reactivation).
    fn observe_boosted(&mut self, gap_secs: f64, alpha_boost: f64);
}

// ── Branch ──────────────────────────────────────────────────────────────────

/// A single conversation branch within a channel.
///
/// Owns its rate estimate and exposes `increment_run_length` /
/// `reset_run_length` for the Bayesian CPD layer to call.
pub struct Branch {
    id: BranchId,
    rate: Box<dyn RateEstimator + Send + Sync>,
    state: BranchState,
    run_length: u32,
    participants: ParticipantSet,
    created_at: Instant,
    last_active_at: Instant,
}

/// Compact participant tracking — stores user IDs seen on this branch.
#[derive(Debug, Clone, Default)]
pub struct ParticipantSet {
    users: Vec<UserId>,
}

impl ParticipantSet {
    fn new() -> Self {
        Self { users: Vec::new() }
    }

    /// Adds a participant if not already present. O(n) but n is small (< 20).
    fn insert(&mut self, user_id: UserId) {
        if !self.users.contains(&user_id) {
            self.users.push(user_id);
        }
    }

    /// Returns the participant slice for Jaccard overlap computation.
    pub fn as_slice(&self) -> &[UserId] {
        &self.users
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

    /// Increment run length. Called by the Bayesian CPD layer when the
    /// current run continues (no change-point detected).
    pub fn increment_run_length(&mut self) {
        self.run_length = self.run_length.saturating_add(1);
    }

    /// Reset run length to zero. Called by the Bayesian CPD layer when
    /// a change-point is detected.
    pub fn reset_run_length(&mut self) {
        self.run_length = 0;
    }

    /// Record a message on this branch, updating rate estimate and timestamps.
    fn record_message(
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
    fn update_dormancy(&mut self, now: Instant, dormancy_multiplier: f64) {
        if self.state == BranchState::New {
            return; // Don't transition New -> Dormant.
        }
        let elapsed = now.duration_since(self.last_active_at).as_secs_f64();
        let threshold = self.rate.estimate() * dormancy_multiplier;
        if elapsed > threshold {
            self.state = BranchState::Dormant;
        } else if self.state == BranchState::Dormant {
            // If we're within threshold, reactivate.
            self.state = BranchState::Active;
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
    /// Confidence in the rate estimate: `min(1.0, observation_count / 10.0)`.
    pub confidence: f32,
    /// Messages since last change-point on this branch.
    pub run_length: u32,
}

// ── Message annotator trait ─────────────────────────────────────────────────

/// Input to the branch tracker: a parsed Discord message with just the fields
/// needed for branch tracking.
pub struct MessageInput<'a> {
    pub channel_id: ChannelId,
    pub message_id: MessageId,
    pub user_id: UserId,
    pub timestamp: Instant,
    /// Content for future topic embedding (not used by EWMA layer).
    pub content: &'a str,
    /// If the message is a reply, the ID of the replied-to message.
    pub reply_to: Option<MessageId>,
}

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

// ── Channel state ───────────────────────────────────────────────────────────

/// Per-channel branch tracking state.
struct ChannelState {
    branches: HashMap<BranchId, Branch>,
    /// Channel-wide composite rate estimate (uses alpha/2 for stability).
    channel_rate: Box<dyn RateEstimator + Send + Sync>,
    /// Next branch ID counter.
    next_branch_id: u64,
    /// Maps message IDs to their branch assignment (ring buffer, capped).
    message_branches: Vec<(MessageId, BranchId)>,
}

/// Maximum message-to-branch mappings kept per channel.
const MESSAGE_BRANCH_CAP: usize = 200;

impl ChannelState {
    fn new(channel_rate: Box<dyn RateEstimator + Send + Sync>) -> Self {
        Self {
            branches: HashMap::new(),
            channel_rate,
            next_branch_id: 0,
            message_branches: Vec::new(),
        }
    }

    /// Access the channel-wide composite rate estimator.
    fn channel_rate(&self) -> &dyn RateEstimator {
        &*self.channel_rate
    }

    /// Allocate a new branch ID.
    fn alloc_branch_id(&mut self) -> BranchId {
        let id = BranchId::new(self.next_branch_id);
        self.next_branch_id += 1;
        id
    }

    /// Record a message -> branch mapping (ring buffer).
    fn record_message_branch(&mut self, message_id: MessageId, branch_id: BranchId) {
        if self.message_branches.len() >= MESSAGE_BRANCH_CAP {
            self.message_branches.remove(0);
        }
        self.message_branches.push((message_id, branch_id));
    }

    /// Look up which branch a message was assigned to.
    fn branch_for_message(&self, message_id: MessageId) -> Option<BranchId> {
        // Search from the end (most recent) for efficiency.
        self.message_branches
            .iter()
            .rev()
            .find(|(mid, _)| *mid == message_id)
            .map(|(_, bid)| *bid)
    }

    /// Find the most recently active non-dormant branch.
    fn most_recent_active_branch(&self) -> Option<BranchId> {
        self.branches
            .values()
            .filter(|b| b.state() != BranchState::Dormant)
            .max_by_key(|b| b.last_active_at())
            .map(|b| b.id())
    }
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

// ── Branch tracker ──────────────────────────────────────────────────────────

/// Top-level branch tracker managing per-channel state.
pub struct BranchTracker {
    channels: HashMap<ChannelId, ChannelState>,
    config: BranchTrackerConfig,
}

impl BranchTracker {
    /// Create a new branch tracker with the given configuration.
    pub fn new(config: BranchTrackerConfig) -> Self {
        Self {
            channels: HashMap::new(),
            config,
        }
    }

    /// Create a new branch tracker with default configuration.
    pub fn with_defaults() -> Self {
        Self::new(BranchTrackerConfig::default())
    }

    /// Access the configuration.
    pub fn config(&self) -> &BranchTrackerConfig {
        &self.config
    }

    /// Number of channels being tracked.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Number of branches in a channel.
    pub fn branch_count(&self, channel_id: ChannelId) -> usize {
        self.channels
            .get(&channel_id)
            .map_or(0, |cs| cs.branches.len())
    }

    /// Get a reference to a branch.
    pub fn branch(&self, channel_id: ChannelId, branch_id: BranchId) -> Option<&Branch> {
        self.channels
            .get(&channel_id)
            .and_then(|cs| cs.branches.get(&branch_id))
    }

    /// Channel-wide composite rate estimate (for CPD/feature vector layers).
    pub fn channel_rate(&self, channel_id: ChannelId) -> Option<&dyn RateEstimator> {
        self.channels.get(&channel_id).map(|cs| cs.channel_rate())
    }

    /// Get a mutable reference to a branch (for CPD layer).
    pub fn branch_mut(
        &mut self,
        channel_id: ChannelId,
        branch_id: BranchId,
    ) -> Option<&mut Branch> {
        self.channels
            .get_mut(&channel_id)
            .and_then(|cs| cs.branches.get_mut(&branch_id))
    }
}

// ── Free functions for split-borrow friendliness ────────────────────────────

/// Ensure a channel state exists, creating it if needed.
/// Takes split borrows to avoid `&mut self` conflicts with config access.
fn ensure_channel<'a>(
    channels: &'a mut HashMap<ChannelId, ChannelState>,
    config: &BranchTrackerConfig,
    channel_id: ChannelId,
) -> &'a mut ChannelState {
    channels.entry(channel_id).or_insert_with(|| {
        let rate = Box::new(ewma::EwmaEstimator::new(
            config.alpha / 2.0,
            config.initial_rate_estimate_secs,
        ));
        ChannelState::new(rate)
    })
}

/// Create a new branch in a channel.
fn create_branch_in(
    channel: &mut ChannelState,
    config: &BranchTrackerConfig,
    now: Instant,
) -> BranchId {
    // Enforce max branches per channel — prune oldest dormant first.
    while channel.branches.len() >= config.max_branches_per_channel {
        let oldest_dormant = channel
            .branches
            .iter()
            .filter(|(_, b)| b.state() == BranchState::Dormant)
            .min_by_key(|(_, b)| b.last_active_at())
            .map(|(id, _)| *id);

        if let Some(id) = oldest_dormant {
            channel.branches.remove(&id);
        } else {
            // No dormant branches — evict the oldest active one.
            let oldest = channel
                .branches
                .iter()
                .min_by_key(|(_, b)| b.last_active_at())
                .map(|(id, _)| *id);
            if let Some(id) = oldest {
                channel.branches.remove(&id);
            } else {
                break;
            }
        }
    }

    let id = channel.alloc_branch_id();
    let rate = Box::new(ewma::EwmaEstimator::new(
        config.alpha,
        config.initial_rate_estimate_secs,
    ));
    let branch = Branch::new(id, rate, now);
    channel.branches.insert(id, branch);
    id
}

/// Create a new rate estimator from config.
fn make_rate_estimator(config: &BranchTrackerConfig) -> Box<dyn RateEstimator + Send + Sync> {
    Box::new(ewma::EwmaEstimator::new(
        config.alpha,
        config.initial_rate_estimate_secs,
    ))
}

impl MessageAnnotator for BranchTracker {
    fn annotate(&mut self, input: &MessageInput<'_>, branch_id: BranchId) -> BranchAnnotation {
        let channel = ensure_channel(&mut self.channels, &self.config, input.channel_id);

        // If the branch doesn't exist yet (new branch from feature vector),
        // create it.
        if let std::collections::hash_map::Entry::Vacant(e) = channel.branches.entry(branch_id) {
            let rate = make_rate_estimator(&self.config);
            let branch = Branch::new(branch_id, rate, input.timestamp);
            e.insert(branch);
            // Advance the counter past this ID if needed.
            if branch_id.raw() >= channel.next_branch_id {
                channel.next_branch_id = branch_id.raw() + 1;
            }
        }

        // Record the message on the branch.
        let dormancy_multiplier = self.config.dormancy_multiplier;
        let reactivation_boost = self.config.reactivation_alpha_boost;
        let branch = channel.branches.get_mut(&branch_id).expect("just ensured");
        branch.record_message(
            input.user_id,
            input.timestamp,
            dormancy_multiplier,
            reactivation_boost,
        );

        // Capture annotation values before borrowing channel again.
        let annotation = BranchAnnotation {
            branch_id,
            branch_state: branch.state(),
            rate_estimate: branch.rate_estimate(),
            confidence: branch.rate_confidence(),
            run_length: branch.run_length(),
        };

        // Feed the branch's observed gap into the channel-wide composite rate.
        let gap = branch.rate_estimate();
        if branch.observation_count() > 0 {
            channel.channel_rate.observe(gap);
        }

        // Record message -> branch mapping.
        channel.record_message_branch(input.message_id, branch_id);

        annotation
    }

    fn assign_default(&mut self, input: &MessageInput<'_>) -> BranchId {
        let channel = ensure_channel(&mut self.channels, &self.config, input.channel_id);

        // Strategy 1: If this is a reply, follow the reply chain.
        if let Some(reply_to) = input.reply_to
            && let Some(branch_id) = channel.branch_for_message(reply_to)
            && channel.branches.contains_key(&branch_id)
        {
            return branch_id;
        }

        // Strategy 2: Assign to the most recently active branch.
        if let Some(branch_id) = channel.most_recent_active_branch() {
            return branch_id;
        }

        // Strategy 3: Create a new branch.
        create_branch_in(channel, &self.config, input.timestamp)
    }

    fn maintain(&mut self, now: Instant) {
        let dormancy_multiplier = self.config.dormancy_multiplier;
        let prune_threshold = self.config.prune_after_secs;

        for channel in self.channels.values_mut() {
            // Update dormancy.
            for branch in channel.branches.values_mut() {
                branch.update_dormancy(now, dormancy_multiplier);
            }
            // Prune expired branches.
            channel.branches.retain(|_, b| {
                now.duration_since(b.last_active_at()).as_secs_f64() < prune_threshold
            });
        }

        // Remove channels with no branches.
        self.channels.retain(|_, cs| !cs.branches.is_empty());
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Helper: create a `MessageInput` with the given parameters.
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
        }
    }

    fn ch(id: u64) -> ChannelId {
        ChannelId::new(id)
    }
    fn mid(id: u64) -> MessageId {
        MessageId::new(id)
    }
    fn uid(id: u64) -> UserId {
        UserId::new(id)
    }

    // ── BranchTracker construction ──────────────────────────────────────

    #[test]
    fn tracker_starts_empty() {
        let tracker = BranchTracker::with_defaults();
        assert_eq!(tracker.channel_count(), 0);
        assert_eq!(tracker.branch_count(ch(1)), 0);
    }

    #[test]
    fn default_config_matches_spec() {
        let config = BranchTrackerConfig::default();
        assert_eq!(config.alpha, 0.2);
        assert_eq!(config.initial_rate_estimate_secs, 60.0);
        assert_eq!(config.dormancy_multiplier, 10.0);
        assert_eq!(config.prune_after_secs, 3600.0);
        assert_eq!(config.max_branches_per_channel, 50);
        assert_eq!(config.reactivation_alpha_boost, 1.5);
    }

    // ── assign_default ──────────────────────────────────────────────────

    #[test]
    fn assign_creates_new_branch_on_first_message() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let branch_id = tracker.assign_default(&input);

        assert_eq!(branch_id.raw(), 0);
        assert_eq!(tracker.channel_count(), 1);
        assert_eq!(tracker.branch_count(ch(100)), 1);
    }

    #[test]
    fn assign_reuses_active_branch() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        // First message creates branch 0.
        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let b1 = tracker.assign_default(&input1);

        // Annotate it to make it active.
        tracker.annotate(&input1, b1);

        // Second message (1s later, no reply) should reuse the active branch.
        let input2 = msg(
            ch(100),
            mid(2),
            uid(43),
            now + Duration::from_secs(1),
            "world",
            None,
        );
        let b2 = tracker.assign_default(&input2);
        assert_eq!(b1, b2, "should reuse the same active branch");
    }

    #[test]
    fn assign_follows_reply_chain() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        // Message 1 on branch 0.
        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let b1 = tracker.assign_default(&input1);
        tracker.annotate(&input1, b1);

        // Message 2 (a reply to message 1) should follow the reply chain.
        let input2 = msg(
            ch(100),
            mid(2),
            uid(43),
            now + Duration::from_secs(5),
            "reply",
            Some(mid(1)),
        );
        let b2 = tracker.assign_default(&input2);
        assert_eq!(b1, b2, "reply should follow to the same branch");
    }

    // ── annotate ────────────────────────────────────────────────────────

    #[test]
    fn annotate_new_branch_produces_correct_annotation() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let branch_id = BranchId::new(0);
        let ann = tracker.annotate(&input, branch_id);

        assert_eq!(ann.branch_id, branch_id);
        // After one message on a new branch, state transitions to Active.
        assert_eq!(ann.branch_state, BranchState::Active);
        assert_eq!(ann.run_length, 0);
    }

    #[test]
    fn annotate_updates_rate_estimate() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        // First message creates the branch. The gap from creation instant
        // to the first message is 0s, which snaps the estimate to 0.0.
        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input1, bid);

        // Second message 10 seconds later.
        // EWMA: alpha=0.2, estimate = 0.2 * 10 + 0.8 * 0 = 2.0
        let input2 = msg(
            ch(100),
            mid(2),
            uid(42),
            now + Duration::from_secs(10),
            "world",
            None,
        );
        let ann2 = tracker.annotate(&input2, bid);
        assert!(
            (ann2.rate_estimate - 2.0).abs() < 0.01,
            "expected ~2.0, got {}",
            ann2.rate_estimate
        );

        // Third message 10s later.
        // EWMA: 0.2 * 10 + 0.8 * 2.0 = 2 + 1.6 = 3.6
        let input3 = msg(
            ch(100),
            mid(3),
            uid(42),
            now + Duration::from_secs(20),
            "again",
            None,
        );
        let ann3 = tracker.annotate(&input3, bid);
        assert!(
            (ann3.rate_estimate - 3.6).abs() < 0.01,
            "expected ~3.6, got {}",
            ann3.rate_estimate
        );
        assert!(ann3.confidence > 0.0);
    }

    #[test]
    fn annotate_creates_branch_if_not_exists() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        // Annotate with a branch ID that doesn't exist yet.
        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(42);
        let ann = tracker.annotate(&input, bid);

        assert_eq!(ann.branch_id, bid);
        assert!(tracker.branch(ch(100), bid).is_some());
    }

    #[test]
    fn annotate_tracks_participants() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();
        let bid = BranchId::new(0);

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        tracker.annotate(&input1, bid);

        let input2 = msg(
            ch(100),
            mid(2),
            uid(43),
            now + Duration::from_secs(1),
            "world",
            None,
        );
        tracker.annotate(&input2, bid);

        let branch = tracker.branch(ch(100), bid).unwrap();
        assert_eq!(branch.participants().len(), 2);
        assert!(branch.participants().as_slice().contains(&uid(42)));
        assert!(branch.participants().as_slice().contains(&uid(43)));
    }

    #[test]
    fn annotate_records_message_branch_mapping() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = tracker.assign_default(&input1);
        tracker.annotate(&input1, bid);

        // Now a reply should be able to find the branch via the mapping.
        let input2 = msg(
            ch(100),
            mid(2),
            uid(43),
            now + Duration::from_secs(1),
            "reply",
            Some(mid(1)),
        );
        let bid2 = tracker.assign_default(&input2);
        assert_eq!(bid, bid2, "reply should follow message-branch mapping");
    }

    // ── Dormancy ────────────────────────────────────────────────────────

    #[test]
    fn branch_goes_dormant_after_threshold() {
        let config = BranchTrackerConfig {
            dormancy_multiplier: 2.0,
            initial_rate_estimate_secs: 10.0,
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input, bid);

        // After 25 seconds (> 10 * 2 = 20s threshold), should be dormant.
        tracker.maintain(now + Duration::from_secs(25));

        let branch = tracker.branch(ch(100), bid).unwrap();
        assert_eq!(branch.state(), BranchState::Dormant);
    }

    #[test]
    fn dormant_branch_reactivates_on_message() {
        let config = BranchTrackerConfig {
            dormancy_multiplier: 2.0,
            initial_rate_estimate_secs: 10.0,
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input1, bid);

        // Go dormant.
        let dormant_time = now + Duration::from_secs(25);
        tracker.maintain(dormant_time);
        assert_eq!(
            tracker.branch(ch(100), bid).unwrap().state(),
            BranchState::Dormant
        );

        // New message reactivates.
        let input2 = msg(ch(100), mid(2), uid(42), dormant_time, "back", None);
        let ann = tracker.annotate(&input2, bid);
        assert_eq!(ann.branch_state, BranchState::Active);
    }

    // ── Pruning ─────────────────────────────────────────────────────────

    #[test]
    fn maintain_prunes_expired_branches() {
        let config = BranchTrackerConfig {
            prune_after_secs: 100.0,
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input, bid);

        // After 101 seconds, should be pruned.
        tracker.maintain(now + Duration::from_secs(101));
        assert_eq!(tracker.branch_count(ch(100)), 0);
        // Channel itself should be removed too.
        assert_eq!(tracker.channel_count(), 0);
    }

    #[test]
    fn max_branches_evicts_dormant_first() {
        let config = BranchTrackerConfig {
            max_branches_per_channel: 3,
            dormancy_multiplier: 2.0,
            initial_rate_estimate_secs: 5.0,
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        // Create 3 branches.
        for i in 0..3u64 {
            let input = msg(
                ch(100),
                mid(i + 1),
                uid(42),
                now + Duration::from_secs(i),
                "msg",
                None,
            );
            let bid = BranchId::new(i);
            tracker.annotate(&input, bid);
        }
        assert_eq!(tracker.branch_count(ch(100)), 3);

        // Make branch 0 dormant.
        tracker.maintain(now + Duration::from_secs(15));

        // Creating a 4th branch should evict dormant branch 0.
        let channel = ensure_channel(&mut tracker.channels, &tracker.config, ch(100));
        let new_bid = create_branch_in(channel, &tracker.config, now + Duration::from_secs(16));

        assert_eq!(tracker.branch_count(ch(100)), 3);
        assert!(tracker.branch(ch(100), BranchId::new(0)).is_none());
        assert!(tracker.branch(ch(100), new_bid).is_some());
    }

    // ── Run length (CPD interface) ──────────────────────────────────────

    #[test]
    fn run_length_increment_and_reset() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input, bid);

        let branch = tracker.branch_mut(ch(100), bid).unwrap();
        assert_eq!(branch.run_length(), 0);

        branch.increment_run_length();
        branch.increment_run_length();
        assert_eq!(branch.run_length(), 2);

        branch.reset_run_length();
        assert_eq!(branch.run_length(), 0);
    }

    #[test]
    fn run_length_saturates() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input, bid);

        let branch = tracker.branch_mut(ch(100), bid).unwrap();
        branch.run_length = u32::MAX - 1;
        branch.increment_run_length();
        assert_eq!(branch.run_length(), u32::MAX);
        branch.increment_run_length();
        assert_eq!(branch.run_length(), u32::MAX);
    }

    // ── Multi-channel ───────────────────────────────────────────────────

    #[test]
    fn separate_channels_are_independent() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let b1 = tracker.assign_default(&input1);
        tracker.annotate(&input1, b1);

        let input2 = msg(ch(200), mid(2), uid(43), now, "world", None);
        let b2 = tracker.assign_default(&input2);
        tracker.annotate(&input2, b2);

        assert_eq!(tracker.channel_count(), 2);
        assert_eq!(tracker.branch_count(ch(100)), 1);
        assert_eq!(tracker.branch_count(ch(200)), 1);

        // Branch IDs are per-channel, so both are 0.
        assert_eq!(b1.raw(), 0);
        assert_eq!(b2.raw(), 0);
    }

    // ── BranchId ────────────────────────────────────────────────────────

    #[test]
    fn branch_id_display() {
        let id = BranchId::new(42);
        assert_eq!(format!("{id}"), "branch/42");
    }

    // ── ParticipantSet ──────────────────────────────────────────────────

    #[test]
    fn participant_set_deduplicates() {
        let mut ps = ParticipantSet::new();
        ps.insert(uid(42));
        ps.insert(uid(42));
        ps.insert(uid(43));
        assert_eq!(ps.len(), 2);
    }

    #[test]
    fn participant_set_empty() {
        let ps = ParticipantSet::new();
        assert!(ps.is_empty());
        assert_eq!(ps.len(), 0);
    }

    // ── Message-branch ring buffer ──────────────────────────────────────

    #[test]
    fn message_branch_ring_buffer_caps() {
        let mut cs = ChannelState::new(Box::new(ewma::EwmaEstimator::new(0.1, 60.0)));
        let bid = BranchId::new(0);

        for i in 1..=(MESSAGE_BRANCH_CAP as u64 + 10) {
            cs.record_message_branch(mid(i), bid);
        }
        assert_eq!(cs.message_branches.len(), MESSAGE_BRANCH_CAP);

        // Oldest entries should have been evicted.
        assert!(cs.branch_for_message(mid(1)).is_none());
        // Newest should exist.
        assert_eq!(
            cs.branch_for_message(mid(MESSAGE_BRANCH_CAP as u64 + 10)),
            Some(bid)
        );
    }

    // ── Channel rate ────────────────────────────────────────────────────

    #[test]
    fn channel_rate_is_exposed() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        assert!(tracker.channel_rate(ch(100)).is_none());

        let input = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input, bid);

        assert!(tracker.channel_rate(ch(100)).is_some());
    }

    // ── BranchState transitions ─────────────────────────────────────────

    #[test]
    fn new_to_active_transition() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let bid = BranchId::new(0);
        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let ann1 = tracker.annotate(&input1, bid);

        // After first observation, should be Active.
        assert_eq!(ann1.branch_state, BranchState::Active);
    }

    #[test]
    fn new_branch_does_not_go_dormant_immediately() {
        let config = BranchTrackerConfig {
            dormancy_multiplier: 0.001, // Extremely aggressive dormancy.
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        let channel = ensure_channel(&mut tracker.channels, &tracker.config, ch(100));
        let bid = create_branch_in(channel, &tracker.config, now);

        // Branch is New, maintain should not make it Dormant.
        tracker.maintain(now + Duration::from_secs(1000));
        let branch = tracker.branch(ch(100), bid);
        // The branch might be pruned due to prune_after_secs=3600, but it
        // shouldn't have transitioned to Dormant — it's New.
        if let Some(b) = branch {
            assert_ne!(b.state(), BranchState::Dormant);
        }
    }
}
