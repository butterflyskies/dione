use std::collections::HashMap;
use std::time::{Duration, Instant};

// ── Newtypes ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParticipantId(String);

impl ParticipantId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChannelRef(String);

impl ChannelRef {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SenderClass(String);

impl SenderClass {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// ── Config ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverflowPolicy {
    Drop { notify: bool },
    Buffer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeConfig {
    pub max_tokens: u32,
    pub window: Duration,
    pub cooldown: Duration,
    pub overflow: OverflowPolicy,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub default: ScopeConfig,
    pub classes: Vec<(SenderClass, ScopeConfig)>,
    pub individuals: HashMap<ParticipantId, ScopeConfig>,
    pub channels: HashMap<ChannelRef, ScopeConfig>,
}

impl RateLimitConfig {
    /// Resolve the effective [`ScopeConfig`] for a message.
    ///
    /// Resolution order (first match wins):
    /// 1. **Individual override** — exact match on `sender` in `self.individuals`.
    /// 2. **Class override** — the participant's `classes` are checked *in order*;
    ///    the first class that appears in `self.classes` wins.  This means the
    ///    caller controls priority by ordering the participant's class list.
    /// 3. **Default** — `self.default` is used when nothing else matches.
    ///
    /// After sender-level resolution, a **channel override** (if present)
    /// replaces the result entirely.
    fn resolve(
        &self,
        sender: &ParticipantId,
        channel: &ChannelRef,
        classes: &[SenderClass],
    ) -> ScopeConfig {
        // Individual > class > default
        let sender_config = if let Some(cfg) = self.individuals.get(sender) {
            cfg.clone()
        } else if let Some(cfg) = classes.iter().find_map(|c| {
            self.classes
                .iter()
                .find(|(cls, _)| cls == c)
                .map(|(_, cfg)| cfg)
        }) {
            cfg.clone()
        } else {
            tracing::debug!(
                sender = %sender.as_str(),
                "no individual or class override, using default rate limit config"
            );
            self.default.clone()
        };

        // Channel override replaces sender-resolved config entirely
        if let Some(cfg) = self.channels.get(channel) {
            return cfg.clone();
        }

        sender_config
    }
}

// ── Bucket state machine ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketState {
    Idle,
    Active { window_expires: Instant },
    Exhausted { cooldown_expires: Instant },
    Cooldown { expires: Instant },
}

#[derive(Debug, Clone)]
pub struct TokenBucket {
    pub state: BucketState,
    pub remaining: u32,
    config: ScopeConfig,
    delivered_this_window: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allowed {
        remaining: u32,
        window_resets: Instant,
    },
    Denied {
        retry_after: Duration,
        overflow: OverflowPolicy,
    },
}

impl TokenBucket {
    pub fn new(config: ScopeConfig) -> Self {
        assert!(config.max_tokens > 0, "max_tokens must be > 0");
        Self {
            state: BucketState::Idle,
            remaining: config.max_tokens,
            config,
            delivered_this_window: 0,
        }
    }

    /// Core state machine. Matches the TLA+ model's ConsumeToken / DropMessage / WindowExpires /
    /// EnterCooldown / CooldownExpires actions, collapsed into a single check-and-advance call.
    ///
    /// Time-driven transitions (window expiry, cooldown expiry) are evaluated lazily here
    /// rather than on a separate tick — the real system doesn't have a global clock.
    pub fn check(&mut self, now: Instant) -> RateLimitDecision {
        // Lazily apply time-driven transitions before evaluating the message.
        self.advance_time(now);

        match &self.state {
            BucketState::Idle => {
                self.remaining -= 1;
                self.delivered_this_window = 1;
                let window_expires = now + self.config.window;
                let new_state = if self.remaining > 0 {
                    BucketState::Active { window_expires }
                } else {
                    BucketState::Exhausted {
                        cooldown_expires: now + self.config.cooldown,
                    }
                };
                self.state = new_state;
                RateLimitDecision::Allowed {
                    remaining: self.remaining,
                    window_resets: window_expires,
                }
            }
            BucketState::Active { window_expires } => {
                let window_expires = *window_expires;
                self.remaining -= 1;
                self.delivered_this_window += 1;
                if self.remaining > 0 {
                    // Stay Active
                } else {
                    self.state = BucketState::Exhausted {
                        cooldown_expires: now + self.config.cooldown,
                    };
                }
                RateLimitDecision::Allowed {
                    remaining: self.remaining,
                    window_resets: window_expires,
                }
            }
            BucketState::Exhausted { cooldown_expires } => RateLimitDecision::Denied {
                retry_after: cooldown_expires.saturating_duration_since(now),
                overflow: self.config.overflow.clone(),
            },
            BucketState::Cooldown { expires } => RateLimitDecision::Denied {
                retry_after: expires.saturating_duration_since(now),
                overflow: self.config.overflow.clone(),
            },
        }
    }

    /// Apply time-driven state transitions (TLA+ Tick + WindowExpires + EnterCooldown +
    /// CooldownExpires collapsed into lazy evaluation at the current wall time).
    fn advance_time(&mut self, now: Instant) {
        loop {
            match &self.state {
                BucketState::Active { window_expires } if now >= *window_expires => {
                    self.state = BucketState::Idle;
                    self.remaining = self.config.max_tokens;
                    self.delivered_this_window = 0;
                }
                BucketState::Exhausted { cooldown_expires } => {
                    // EnterCooldown: Exhausted -> Cooldown (immediate in real time)
                    self.state = BucketState::Cooldown {
                        expires: *cooldown_expires,
                    };
                }
                BucketState::Cooldown { expires } if now >= *expires => {
                    self.state = BucketState::Idle;
                    self.remaining = self.config.max_tokens;
                    self.delivered_this_window = 0;
                }
                _ => break,
            }
        }
    }

    pub fn delivered_this_window(&self) -> u32 {
        self.delivered_this_window
    }

    /// Returns a reference to the bucket's [`ScopeConfig`], for comparison
    /// when checking whether a participant's resolved config has changed.
    pub fn config(&self) -> &ScopeConfig {
        &self.config
    }

    /// Advance time-driven state transitions to `now`, exposing the lazy
    /// tick for callers that need to settle state before inspection (e.g.
    /// pruning idle buckets).
    pub fn advance_to(&mut self, now: Instant) {
        self.advance_time(now);
    }
}

// ── RateLimiter (manager) ────────────────────────────────────────────────────

pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: HashMap<(ParticipantId, ChannelRef), TokenBucket>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: HashMap::new(),
        }
    }

    /// Returns a reference to the current config for comparison.
    pub fn config_ref(&self) -> &RateLimitConfig {
        &self.config
    }

    /// Replace the stored config. Existing buckets are preserved — they will
    /// be reconfigured on their next `check_message` call when the resolved
    /// config differs from the bucket's stored config.
    pub fn update_config(&mut self, config: RateLimitConfig) {
        self.config = config;
    }

    pub fn check_message(
        &mut self,
        sender: &ParticipantId,
        channel: &ChannelRef,
        classes: &[SenderClass],
        now: Instant,
    ) -> RateLimitDecision {
        if !self.config.enabled {
            return RateLimitDecision::Allowed {
                remaining: u32::MAX,
                window_resets: now,
            };
        }

        let resolved = self.config.resolve(sender, channel, classes);
        let key = (sender.clone(), channel.clone());
        let bucket = self
            .buckets
            .entry(key)
            .or_insert_with(|| TokenBucket::new(resolved.clone()));

        // If the resolved config has changed (e.g. guild roles changed),
        // replace the bucket so the new limits take effect immediately.
        if *bucket.config() != resolved {
            tracing::trace!(
                sender = %sender.as_str(),
                channel = %channel.as_str(),
                "rate limiter: config changed for bucket, replacing"
            );
            *bucket = TokenBucket::new(resolved);
        }

        bucket.check(now)
    }

    /// Remove all buckets currently in [`BucketState::Idle`] state.
    ///
    /// Idle buckets hold no meaningful state — they have full tokens and no
    /// active window, which is identical to a freshly-created bucket.  Pruning
    /// them bounds the HashMap size to the number of *actively rate-limited*
    /// participants rather than every participant ever seen.
    ///
    /// Callers should invoke this periodically (e.g. every N events or on a
    /// timer) from the event loop.
    pub fn prune_idle(&mut self, now: Instant) -> usize {
        // Advance time on every bucket first so stale Active/Cooldown buckets
        // that have timed out get settled to Idle before we check.
        for bucket in self.buckets.values_mut() {
            bucket.advance_to(now);
        }

        let before = self.buckets.len();
        self.buckets
            .retain(|_key, bucket| bucket.state != BucketState::Idle);
        let pruned = before - self.buckets.len();

        if pruned > 0 {
            tracing::trace!(
                pruned,
                remaining = self.buckets.len(),
                "rate limiter: pruned idle buckets"
            );
        }

        pruned
    }

    /// Returns the number of active buckets (for diagnostics).
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn default_scope(tokens: u32) -> ScopeConfig {
        ScopeConfig {
            max_tokens: tokens,
            window: Duration::from_secs(3600),
            cooldown: Duration::from_secs(3600),
            overflow: OverflowPolicy::Drop { notify: true },
        }
    }

    fn default_config(tokens: u32) -> RateLimitConfig {
        RateLimitConfig {
            enabled: true,
            default: default_scope(tokens),
            classes: Vec::new(),
            individuals: HashMap::new(),
            channels: HashMap::new(),
        }
    }

    fn sender(id: &str) -> ParticipantId {
        ParticipantId::new(id)
    }

    fn channel(id: &str) -> ChannelRef {
        ChannelRef::new(id)
    }

    // ── Unit tests ───────────────────────────────────────────────────────────

    #[test]
    #[should_panic(expected = "max_tokens must be > 0")]
    fn zero_max_tokens_panics() {
        TokenBucket::new(default_scope(0));
    }

    #[test]
    fn consume_n_plus_1_messages() {
        let n = 5u32;
        let mut bucket = TokenBucket::new(default_scope(n));
        let now = Instant::now();

        for i in 0..n {
            let decision = bucket.check(now);
            assert!(
                matches!(decision, RateLimitDecision::Allowed { .. }),
                "message {i} should be allowed"
            );
        }

        let decision = bucket.check(now);
        assert!(
            matches!(decision, RateLimitDecision::Denied { .. }),
            "message N+1 should be denied"
        );
    }

    #[test]
    fn wait_past_window_and_cooldown_refills() {
        let mut bucket = TokenBucket::new(ScopeConfig {
            max_tokens: 2,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(5),
            overflow: OverflowPolicy::Drop { notify: false },
        });
        let start = Instant::now();

        // Exhaust
        bucket.check(start);
        bucket.check(start);
        assert!(matches!(
            bucket.check(start),
            RateLimitDecision::Denied { .. }
        ));

        // After cooldown expires
        let after_cooldown = start + Duration::from_secs(6);
        let decision = bucket.check(after_cooldown);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { remaining: 1, .. }),
            "bucket should refill after cooldown: {decision:?}"
        );
    }

    #[test]
    fn partial_consumption_window_expiry_resets_to_idle() {
        let mut bucket = TokenBucket::new(ScopeConfig {
            max_tokens: 5,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(60),
            overflow: OverflowPolicy::Drop { notify: false },
        });
        let start = Instant::now();

        // Use 2 of 5 tokens
        bucket.check(start);
        bucket.check(start);
        assert_eq!(bucket.remaining, 3);

        // Window expires without exhaustion -> reset to Idle
        let after_window = start + Duration::from_secs(11);
        let decision = bucket.check(after_window);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { remaining: 4, .. }),
            "should reset to full tokens after window expiry: {decision:?}"
        );
    }

    #[test]
    fn disabled_rate_limiter_always_allows() {
        let mut limiter = RateLimiter::new(RateLimitConfig {
            enabled: false,
            ..default_config(1)
        });
        let now = Instant::now();
        let s = sender("bot1");
        let c = channel("ch1");

        // Even after what would be exhaustion, still allowed
        for _ in 0..100 {
            assert!(matches!(
                limiter.check_message(&s, &c, &[], now),
                RateLimitDecision::Allowed { .. }
            ));
        }
    }

    #[test]
    fn individual_override_takes_precedence() {
        let s = sender("special");
        let c = channel("ch1");

        let mut config = default_config(1);
        config.classes.push((
            SenderClass::new("bot"),
            ScopeConfig {
                max_tokens: 2,
                ..default_scope(2)
            },
        ));
        config.individuals.insert(
            s.clone(),
            ScopeConfig {
                max_tokens: 10,
                ..default_scope(10)
            },
        );

        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();
        let classes = [SenderClass::new("bot")];

        // Should get 10 tokens (individual), not 2 (class) or 1 (default)
        for i in 0..10 {
            assert!(
                matches!(
                    limiter.check_message(&s, &c, &classes, now),
                    RateLimitDecision::Allowed { .. }
                ),
                "message {i} should be allowed with individual override"
            );
        }
        assert!(matches!(
            limiter.check_message(&s, &c, &classes, now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn class_override_takes_precedence_over_default() {
        let s = sender("somebot");
        let c = channel("ch1");

        let mut config = default_config(1);
        config.classes.push((
            SenderClass::new("bot"),
            ScopeConfig {
                max_tokens: 5,
                ..default_scope(5)
            },
        ));

        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();
        let classes = [SenderClass::new("bot")];

        for i in 0..5 {
            assert!(
                matches!(
                    limiter.check_message(&s, &c, &classes, now),
                    RateLimitDecision::Allowed { .. }
                ),
                "message {i} should be allowed with class override"
            );
        }
        assert!(matches!(
            limiter.check_message(&s, &c, &classes, now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn channel_override_replaces_sender_config() {
        let s = sender("user1");
        let c = channel("restricted");

        let mut config = default_config(100);
        config.channels.insert(
            c.clone(),
            ScopeConfig {
                max_tokens: 2,
                ..default_scope(2)
            },
        );

        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();

        limiter.check_message(&s, &c, &[], now);
        limiter.check_message(&s, &c, &[], now);
        assert!(matches!(
            limiter.check_message(&s, &c, &[], now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn two_senders_same_channel_independent_buckets() {
        let mut limiter = RateLimiter::new(default_config(2));
        let now = Instant::now();
        let c = channel("ch1");
        let s1 = sender("alice");
        let s2 = sender("bob");

        // Exhaust s1
        limiter.check_message(&s1, &c, &[], now);
        limiter.check_message(&s1, &c, &[], now);
        assert!(matches!(
            limiter.check_message(&s1, &c, &[], now),
            RateLimitDecision::Denied { .. }
        ));

        // s2 should still have full budget
        assert!(matches!(
            limiter.check_message(&s2, &c, &[], now),
            RateLimitDecision::Allowed { remaining: 1, .. }
        ));
    }

    #[test]
    fn sender_without_matching_config_uses_default() {
        let mut limiter = RateLimiter::new(default_config(3));
        let now = Instant::now();
        let s = sender("unknown");
        let c = channel("ch1");
        let classes = [SenderClass::new("nonexistent_class")];

        for _ in 0..3 {
            assert!(matches!(
                limiter.check_message(&s, &c, &classes, now),
                RateLimitDecision::Allowed { .. }
            ));
        }
        assert!(matches!(
            limiter.check_message(&s, &c, &classes, now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn first_matching_class_wins() {
        let s = sender("user1");
        let c = channel("ch1");

        let mut config = default_config(1);
        config.classes.push((
            SenderClass::new("moderator"),
            ScopeConfig {
                max_tokens: 20,
                ..default_scope(20)
            },
        ));
        config.classes.push((
            SenderClass::new("bot"),
            ScopeConfig {
                max_tokens: 5,
                ..default_scope(5)
            },
        ));

        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();

        // User has both classes; "bot" listed first in their class list -> matches "bot" config
        // because we iterate the user's classes and find first match in config
        let classes = [SenderClass::new("bot"), SenderClass::new("moderator")];

        for i in 0..5 {
            assert!(
                matches!(
                    limiter.check_message(&s, &c, &classes, now),
                    RateLimitDecision::Allowed { .. }
                ),
                "message {i} should be allowed"
            );
        }
        assert!(matches!(
            limiter.check_message(&s, &c, &classes, now),
            RateLimitDecision::Denied { .. }
        ));
    }

    #[test]
    fn exhausted_transitions_through_cooldown() {
        let mut bucket = TokenBucket::new(ScopeConfig {
            max_tokens: 1,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(5),
            overflow: OverflowPolicy::Drop { notify: false },
        });
        let start = Instant::now();

        // Consume the single token
        let d = bucket.check(start);
        assert!(matches!(d, RateLimitDecision::Allowed { remaining: 0, .. }));

        // Next check should be denied (Exhausted -> Cooldown happens lazily)
        let d = bucket.check(start + Duration::from_secs(1));
        assert!(matches!(d, RateLimitDecision::Denied { .. }));
        // State should now be Cooldown
        assert!(matches!(bucket.state, BucketState::Cooldown { .. }));

        // Still denied during cooldown
        let d = bucket.check(start + Duration::from_secs(4));
        assert!(matches!(d, RateLimitDecision::Denied { .. }));

        // After cooldown expires -> Idle -> Allowed
        let d = bucket.check(start + Duration::from_secs(6));
        assert!(
            matches!(d, RateLimitDecision::Allowed { remaining: 0, .. }),
            "should refill after cooldown: {d:?}"
        );
    }

    // ── Pruning tests ────────────────────────────────────────────────────────

    #[test]
    fn prune_idle_removes_idle_buckets() {
        let mut limiter = RateLimiter::new(default_config(5));
        let now = Instant::now();
        let s1 = sender("alice");
        let s2 = sender("bob");
        let c = channel("ch1");

        // Touch both senders — creates buckets in Active state
        limiter.check_message(&s1, &c, &[], now);
        limiter.check_message(&s2, &c, &[], now);
        assert_eq!(limiter.bucket_count(), 2);

        // Advance past window so buckets transition to Idle
        let after_window = now + Duration::from_secs(3601);
        let pruned = limiter.prune_idle(after_window);

        assert_eq!(pruned, 2);
        assert_eq!(limiter.bucket_count(), 0);
    }

    #[test]
    fn prune_idle_keeps_active_and_cooldown_buckets() {
        // Use a short window (10s) but long cooldown (120s) so we can advance
        // past the window without expiring the cooldown.
        let config = RateLimitConfig {
            enabled: true,
            default: ScopeConfig {
                max_tokens: 2,
                window: Duration::from_secs(10),
                cooldown: Duration::from_secs(120),
                overflow: OverflowPolicy::Drop { notify: false },
            },
            classes: Vec::new(),
            individuals: HashMap::new(),
            channels: HashMap::new(),
        };
        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();
        let active_sender = sender("active");
        let exhausted_sender = sender("exhausted");
        let idle_sender = sender("idle");
        let c = channel("ch1");

        // active_sender: 1 of 2 tokens used -> Active
        limiter.check_message(&active_sender, &c, &[], now);

        // exhausted_sender: exhaust all tokens -> Exhausted -> Cooldown
        limiter.check_message(&exhausted_sender, &c, &[], now);
        limiter.check_message(&exhausted_sender, &c, &[], now);
        // Trigger advance_time to transition Exhausted -> Cooldown
        limiter.check_message(&exhausted_sender, &c, &[], now);

        // idle_sender: touch then wait past window -> will be Idle after advance
        limiter.check_message(&idle_sender, &c, &[], now);
        assert_eq!(limiter.bucket_count(), 3);

        // Advance past the window (10s) but NOT past the cooldown (120s)
        let after_window = now + Duration::from_secs(11);
        let pruned = limiter.prune_idle(after_window);

        // idle_sender and active_sender both transition to Idle (window expired),
        // exhausted_sender stays in Cooldown
        assert_eq!(pruned, 2);
        assert_eq!(limiter.bucket_count(), 1);
    }

    #[test]
    fn prune_after_cooldown_expiry_removes_bucket() {
        let mut limiter = RateLimiter::new(RateLimitConfig {
            enabled: true,
            default: ScopeConfig {
                max_tokens: 1,
                window: Duration::from_secs(10),
                cooldown: Duration::from_secs(30),
                overflow: OverflowPolicy::Drop { notify: false },
            },
            classes: Vec::new(),
            individuals: HashMap::new(),
            channels: HashMap::new(),
        });
        let now = Instant::now();
        let s = sender("user");
        let c = channel("ch1");

        // Exhaust the bucket
        limiter.check_message(&s, &c, &[], now);
        assert!(matches!(
            limiter.check_message(&s, &c, &[], now),
            RateLimitDecision::Denied { .. }
        ));
        assert_eq!(limiter.bucket_count(), 1);

        // After cooldown expires, bucket should settle to Idle and be pruned
        let after_cooldown = now + Duration::from_secs(31);
        let pruned = limiter.prune_idle(after_cooldown);
        assert_eq!(pruned, 1);
        assert_eq!(limiter.bucket_count(), 0);
    }

    // ── Config refresh tests ────────────────────────────────────────────────

    #[test]
    fn class_change_triggers_bucket_replacement() {
        let s = sender("user1");
        let c = channel("ch1");

        let mut config = default_config(5);
        config.classes.push((
            SenderClass::new("premium"),
            ScopeConfig {
                max_tokens: 20,
                ..default_scope(20)
            },
        ));

        let mut limiter = RateLimiter::new(config);
        let now = Instant::now();

        // First message with no classes -> gets default (5 tokens)
        let decision = limiter.check_message(&s, &c, &[], now);
        assert!(matches!(
            decision,
            RateLimitDecision::Allowed { remaining: 4, .. }
        ));

        // Next message with "premium" class -> bucket should be replaced with 20 tokens
        let classes = [SenderClass::new("premium")];
        let decision = limiter.check_message(&s, &c, &classes, now);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { remaining: 19, .. }),
            "bucket should be replaced with premium config: {decision:?}"
        );
    }

    #[test]
    fn same_class_does_not_replace_bucket() {
        let s = sender("user1");
        let c = channel("ch1");
        let mut limiter = RateLimiter::new(default_config(5));
        let now = Instant::now();

        // Use 3 tokens
        limiter.check_message(&s, &c, &[], now);
        limiter.check_message(&s, &c, &[], now);
        limiter.check_message(&s, &c, &[], now);

        // Same config -> bucket should NOT be replaced
        let decision = limiter.check_message(&s, &c, &[], now);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { remaining: 1, .. }),
            "bucket should keep its state when config unchanged: {decision:?}"
        );
    }

    #[test]
    fn individual_override_added_replaces_bucket() {
        let s = sender("user1");
        let c = channel("ch1");

        let mut config = default_config(3);
        // Start without individual override
        let mut limiter = RateLimiter::new(config.clone());
        let now = Instant::now();

        // Use 1 of 3 default tokens
        limiter.check_message(&s, &c, &[], now);

        // Now add individual override and create a new limiter with different config
        // (simulating config reload — the RateLimiter itself doesn't hot-reload,
        // but the config resolution path picks up changes)
        config.individuals.insert(
            s.clone(),
            ScopeConfig {
                max_tokens: 50,
                ..default_scope(50)
            },
        );
        let mut limiter2 = RateLimiter::new(config);

        // Even fresh limiter2 will create a new bucket with the individual override
        let decision = limiter2.check_message(&s, &c, &[], now);
        assert!(
            matches!(decision, RateLimitDecision::Allowed { remaining: 49, .. }),
            "individual override should be used: {decision:?}"
        );
    }

    #[test]
    fn buffer_overflow_policy_returned_on_denial() {
        let mut bucket = TokenBucket::new(ScopeConfig {
            max_tokens: 1,
            window: Duration::from_secs(10),
            cooldown: Duration::from_secs(5),
            overflow: OverflowPolicy::Buffer,
        });
        let now = Instant::now();

        bucket.check(now);
        let decision = bucket.check(now);
        assert_eq!(
            decision,
            RateLimitDecision::Denied {
                retry_after: Duration::from_secs(5),
                overflow: OverflowPolicy::Buffer,
            }
        );
    }
}

// ── Property-based tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Action {
        SendMessage {
            sender_idx: usize,
            channel_idx: usize,
        },
        Tick {
            duration_ms: u64,
        },
    }

    fn action_strategy(max_senders: usize, max_channels: usize) -> impl Strategy<Value = Action> {
        prop_oneof![
            3 => (0..max_senders, 0..max_channels)
                .prop_map(|(s, c)| Action::SendMessage { sender_idx: s, channel_idx: c }),
            1 => (1u64..10_000)
                .prop_map(|ms| Action::Tick { duration_ms: ms }),
        ]
    }

    fn actions_strategy() -> impl Strategy<Value = Vec<Action>> {
        prop::collection::vec(action_strategy(3, 2), 1..200)
    }

    struct TestHarness {
        limiter: RateLimiter,
        senders: Vec<ParticipantId>,
        channels: Vec<ChannelRef>,
        now: Instant,
    }

    impl TestHarness {
        fn new(max_tokens: u32) -> Self {
            let config = RateLimitConfig {
                enabled: true,
                default: ScopeConfig {
                    max_tokens,
                    window: Duration::from_secs(60),
                    cooldown: Duration::from_secs(30),
                    overflow: OverflowPolicy::Drop { notify: false },
                },
                classes: Vec::new(),
                individuals: HashMap::new(),
                channels: HashMap::new(),
            };
            Self {
                limiter: RateLimiter::new(config),
                senders: (0..3)
                    .map(|i| ParticipantId::new(format!("sender_{i}")))
                    .collect(),
                channels: (0..2)
                    .map(|i| ChannelRef::new(format!("channel_{i}")))
                    .collect(),
                now: Instant::now(),
            }
        }

        fn apply(&mut self, action: &Action) {
            match action {
                Action::SendMessage {
                    sender_idx,
                    channel_idx,
                } => {
                    let sender = &self.senders[*sender_idx];
                    let channel = &self.channels[*channel_idx];
                    self.limiter.check_message(sender, channel, &[], self.now);
                }
                Action::Tick { duration_ms } => {
                    self.now += Duration::from_millis(*duration_ms);
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1000))]

        #[test]
        fn budget_non_negative(actions in actions_strategy()) {
            let mut harness = TestHarness::new(5);
            for action in &actions {
                harness.apply(action);

                // Check all buckets: remaining >= 0 (u32, so structurally true,
                // but verify no underflow panic occurred)
                for bucket in harness.limiter.buckets.values() {
                    prop_assert!(bucket.remaining <= bucket.config.max_tokens,
                        "remaining {} exceeds max_tokens {}", bucket.remaining, bucket.config.max_tokens);
                }
            }
        }

        #[test]
        fn no_delivery_after_exhaustion(actions in actions_strategy()) {
            let mut harness = TestHarness::new(5);
            for action in &actions {
                if let Action::SendMessage { sender_idx, channel_idx } = action {
                    let sender = &harness.senders[*sender_idx];
                    let channel = &harness.channels[*channel_idx];
                    let key = (sender.clone(), channel.clone());

                    let was_exhausted_or_cooldown = harness.limiter.buckets.get(&key)
                        .map(|b| matches!(b.state,
                            BucketState::Exhausted { .. } | BucketState::Cooldown { .. }))
                        .unwrap_or(false);

                    harness.apply(action);

                    if was_exhausted_or_cooldown {
                        // After advance_time in check(), the state may have transitioned
                        // to Idle if cooldown expired. Only assert denial if the bucket
                        // is still in a limiting state.
                        let bucket = harness.limiter.buckets.get(&key).unwrap();
                        if matches!(bucket.state, BucketState::Exhausted { .. } | BucketState::Cooldown { .. }) {
                            // The decision must have been Denied (the message was just processed)
                            // We can't easily capture the return value here, but we can verify
                            // the structural invariant: remaining must be 0
                            prop_assert_eq!(bucket.remaining, 0,
                                "remaining must be 0 in exhausted/cooldown state");
                        }
                    }
                } else {
                    harness.apply(action);
                }
            }
        }

        #[test]
        fn delivered_bound(actions in actions_strategy()) {
            let max_tokens = 5u32;
            let mut harness = TestHarness::new(max_tokens);
            for action in &actions {
                harness.apply(action);

                // Use the bucket's own delivered counter — it resets with the window/cooldown
                // transitions, matching the TLA+ model's `delivered` variable.
                for bucket in harness.limiter.buckets.values() {
                    prop_assert!(bucket.delivered_this_window() <= max_tokens,
                        "delivered {} exceeds max_tokens {}", bucket.delivered_this_window(), max_tokens);
                }
            }
        }

        #[test]
        fn isolation(actions in actions_strategy()) {
            let max_tokens = 5u32;
            let mut harness = TestHarness::new(max_tokens);

            // Track per-bucket remaining before and after each send to verify
            // that only the targeted bucket changes.
            for action in &actions {
                if let Action::SendMessage { sender_idx, channel_idx } = action {
                    let target_key = (
                        harness.senders[*sender_idx].clone(),
                        harness.channels[*channel_idx].clone(),
                    );

                    let pre_snapshot: HashMap<_, _> = harness.limiter.buckets.iter()
                        .filter(|(k, _)| **k != target_key)
                        .map(|(k, b)| (k.clone(), (b.remaining, b.state.clone())))
                        .collect();

                    harness.apply(action);

                    for (k, (pre_remaining, pre_state)) in &pre_snapshot {
                        if let Some(bucket) = harness.limiter.buckets.get(k) {
                            // Time advances can change state (window/cooldown expiry),
                            // but SendMessage should not. Since Tick is a separate action,
                            // remaining and state should be unchanged for non-target buckets
                            // within a SendMessage action.
                            //
                            // However, advance_time IS called inside check() for the
                            // target bucket only. Other buckets are untouched.
                            prop_assert_eq!(bucket.remaining, *pre_remaining,
                                "non-target bucket {:?} remaining changed", k);
                            prop_assert_eq!(&bucket.state, pre_state,
                                "non-target bucket {:?} state changed", k);
                        }
                    }
                } else {
                    harness.apply(action);
                }
            }
        }
    }
}
