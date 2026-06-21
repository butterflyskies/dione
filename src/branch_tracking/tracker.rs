//! Branch tracker: per-channel state management and message annotation.

use std::collections::HashMap;
use std::time::Instant;

use serenity::model::id::{ChannelId, MessageId};

use super::ewma;
use super::types::{
    Branch, BranchAnnotation, BranchId, BranchState, BranchTrackerConfig, MessageAnnotator,
    MessageInput, RateEstimator,
};

// ── Channel state ───────────────────────────────────────────────────────────

/// Per-channel branch tracking state.
pub(crate) struct ChannelState {
    pub(crate) branches: HashMap<BranchId, Branch>,
    /// Channel-wide composite rate estimate (uses alpha/2 for stability).
    channel_rate: Box<dyn RateEstimator + Send + Sync>,
    /// Next branch ID counter.
    pub(crate) next_branch_id: u64,
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

// ── Branch tracker ──────────────────────────────────────────────────────────

/// Top-level branch tracker managing per-channel state.
pub struct BranchTracker {
    pub(crate) channels: HashMap<ChannelId, ChannelState>,
    pub(crate) config: BranchTrackerConfig,
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

    /// Look up which branch a message was assigned to (for reply-chain matching).
    pub fn branch_for_message(
        &self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> Option<BranchId> {
        self.channels
            .get(&channel_id)
            .and_then(|cs| cs.branch_for_message(message_id))
    }

    /// Iterate over all branches in a channel (for feature vector scoring).
    pub fn branches_in_channel(
        &self,
        channel_id: ChannelId,
    ) -> impl Iterator<Item = (&BranchId, &Branch)> {
        self.channels
            .get(&channel_id)
            .into_iter()
            .flat_map(|cs| cs.branches.iter())
    }
}

// ── Free functions for split-borrow friendliness ────────────────────────────

/// Ensure a channel state exists, creating it if needed.
/// Takes split borrows to avoid `&mut self` conflicts with config access.
pub(crate) fn ensure_channel<'a>(
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
pub(crate) fn create_branch_in(
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

        // Ensure the branch exists (new branch from feature vector) and get
        // a mutable reference in one step — no panic path.
        let dormancy_multiplier = self.config.dormancy_multiplier;
        let reactivation_boost = self.config.reactivation_alpha_boost;
        let branch = channel.branches.entry(branch_id).or_insert_with(|| {
            // Advance the counter past this ID if needed.
            if branch_id.raw() >= channel.next_branch_id {
                channel.next_branch_id = branch_id.raw() + 1;
            }
            let rate = make_rate_estimator(&self.config);
            Branch::new(branch_id, rate, input.timestamp)
        });
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

    use serenity::model::id::UserId;

    use super::super::types::ParticipantSet;

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
            mentions: Vec::new(),
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

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let b1 = tracker.assign_default(&input1);
        tracker.annotate(&input1, b1);

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

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let b1 = tracker.assign_default(&input1);
        tracker.annotate(&input1, b1);

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
        assert_eq!(ann.branch_state, BranchState::Active);
        assert_eq!(ann.run_length, 0);
    }

    #[test]
    fn annotate_updates_rate_estimate() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

        let input1 = msg(ch(100), mid(1), uid(42), now, "hello", None);
        let bid = BranchId::new(0);
        tracker.annotate(&input1, bid);

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
    }

    #[test]
    fn annotate_creates_branch_if_not_exists() {
        let mut tracker = BranchTracker::with_defaults();
        let now = Instant::now();

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

        let dormant_time = now + Duration::from_secs(25);
        tracker.maintain(dormant_time);
        assert_eq!(
            tracker.branch(ch(100), bid).unwrap().state(),
            BranchState::Dormant
        );

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

        tracker.maintain(now + Duration::from_secs(101));
        assert_eq!(tracker.branch_count(ch(100)), 0);
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

        tracker.maintain(now + Duration::from_secs(15));

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

        assert!(cs.branch_for_message(mid(1)).is_none());
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

        assert_eq!(ann1.branch_state, BranchState::Active);
    }

    #[test]
    fn new_branch_does_not_go_dormant_immediately() {
        let config = BranchTrackerConfig {
            dormancy_multiplier: 0.001,
            ..Default::default()
        };
        let mut tracker = BranchTracker::new(config);
        let now = Instant::now();

        let channel = ensure_channel(&mut tracker.channels, &tracker.config, ch(100));
        let bid = create_branch_in(channel, &tracker.config, now);

        tracker.maintain(now + Duration::from_secs(1000));
        let branch = tracker.branch(ch(100), bid);
        if let Some(b) = branch {
            assert_ne!(b.state(), BranchState::Dormant);
        }
    }
}
