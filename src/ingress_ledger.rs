//! Bounded Discord lifecycle admission ledger.
//!
//! Identity caches answer provider lookups; this ledger answers whether a
//! create/edit/delete belongs to an already admitted Discord message lineage.
//! Active admissions remain available for passive lifecycle events for seven
//! days, subject to oldest-first capacity eviction. Terminal tombstones remain
//! for one day. Expiry only removes passive-event availability; it never
//! changes a prior delivery decision.
//!
//! Discord exposes no authenticated correlation between a provider's deleted
//! original message and its later webhook repost. Distinct message IDs therefore
//! remain independent lineages: the ledger does not suppress either event based
//! on timing or content similarity.

use crate::discord::verified_action::{
    LifecycleAdmissionFacts, LifecycleContext, LifecycleProvenance,
};
use auspex_core::{ChannelRef, ContentHash};
use serenity::model::{
    Timestamp,
    id::{ChannelId, MessageId, UserId, WebhookId},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const ACTIVE_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const TOMBSTONE_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_MAX_ACTIVE: usize = 16_384;
const DEFAULT_MAX_TOMBSTONES: usize = 16_384;

trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug)]
enum StoredLineage {
    Direct,
    Verified(LifecycleAdmissionFacts),
}

impl StoredLineage {
    fn same_actor_lineage(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Direct, Self::Direct) => true,
            (Self::Verified(left), Self::Verified(right)) => left.same_actor_lineage(right),
            (Self::Direct, Self::Verified(_)) | (Self::Verified(_), Self::Direct) => false,
        }
    }
}

#[derive(Debug)]
struct ActiveRecord {
    channel_id: ChannelId,
    context: LifecycleContext,
    actor_id: UserId,
    thread_parent_id: Option<ChannelId>,
    content_hash: ContentHash,
    last_version: Timestamp,
    admission_order: u64,
    expires_at: Instant,
    lineage: StoredLineage,
}

#[derive(Debug)]
struct Tombstone {
    channel_id: ChannelId,
    context: LifecycleContext,
    lineage_confirmed: bool,
    terminal_at: Instant,
    expires_at: Instant,
}

impl Tombstone {
    fn binding_matches(&self, channel_id: ChannelId, context: LifecycleContext) -> bool {
        self.channel_id == channel_id && self.context == context
    }
}

struct LedgerState {
    active: HashMap<MessageId, ActiveRecord>,
    tombstones: HashMap<MessageId, Tombstone>,
    next_admission_order: u64,
}

impl Default for LedgerState {
    fn default() -> Self {
        Self {
            active: HashMap::new(),
            tombstones: HashMap::new(),
            next_admission_order: 1,
        }
    }
}

/// Result of verifying a message against active lifecycle admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    Admitted {
        channel: ChannelRef,
    },
    Unknown,
    Expired,
    ChannelMismatch {
        admitted_channel: ChannelRef,
        claimed_channel: ChannelRef,
    },
    Unavailable,
}

/// Immutable delivery facts returned only by an admitted transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LifecycleSnapshot {
    channel_id: ChannelId,
    message_id: MessageId,
    context: LifecycleContext,
    actor_id: UserId,
    thread_parent_id: Option<ChannelId>,
    provenance: Option<LifecycleProvenance>,
}

impl LifecycleSnapshot {
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }

    pub(crate) fn thread_parent_id(&self) -> Option<ChannelId> {
        self.thread_parent_id
    }

    /// The participant identity exposed to downstream delivery consumers.
    ///
    /// Verified represented actions collapse to the represented Discord user;
    /// direct, app-only, and unavailable actions retain the observed author.
    pub(crate) fn effective_user_id(&self) -> UserId {
        match self.provenance.as_ref() {
            Some(LifecycleProvenance::Represented {
                discord_user_id, ..
            }) => *discord_user_id,
            Some(LifecycleProvenance::AppOnly | LifecycleProvenance::Unavailable(_)) | None => {
                self.actor_id
            }
        }
    }

    pub(crate) fn provenance(&self) -> Option<&LifecycleProvenance> {
        self.provenance.as_ref()
    }
}

/// Borrowed lineage supplied to pure current-policy edit checks.
pub(crate) struct LifecycleView<'a> {
    record: &'a ActiveRecord,
}

impl LifecycleView<'_> {
    pub(crate) fn actor_id(&self) -> UserId {
        self.record.actor_id
    }
    pub(crate) fn provenance(&self) -> Option<&LifecycleProvenance> {
        match &self.record.lineage {
            StoredLineage::Direct => None,
            StoredLineage::Verified(facts) => Some(facts.provenance()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TransitionResult {
    Admitted(LifecycleSnapshot),
    Duplicate,
    Rejected,
    Unavailable,
}

pub struct IngressLedger {
    state: Mutex<LedgerState>,
    clock: Arc<dyn Clock>,
    active_retention: Duration,
    tombstone_retention: Duration,
    max_active: usize,
    max_tombstones: usize,
    epoch: Instant,
    #[cfg(test)]
    observed_verifications: Mutex<Vec<VerifyResult>>,
}

impl Default for IngressLedger {
    fn default() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let epoch = clock.now();
        Self {
            state: Mutex::new(LedgerState::default()),
            clock,
            active_retention: ACTIVE_RETENTION,
            tombstone_retention: TOMBSTONE_RETENTION,
            max_active: DEFAULT_MAX_ACTIVE,
            max_tombstones: DEFAULT_MAX_TOMBSTONES,
            epoch,
            #[cfg(test)]
            observed_verifications: Mutex::new(Vec::new()),
        }
    }
}

impl IngressLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    fn hash_content(content: &str) -> ContentHash {
        auspex_core::ingress_ledger::IngressLedger::hash_content(content)
    }

    /// Compatibility constructor for direct admitted Discord messages.
    #[cfg(test)]
    pub fn note_admitted(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        user_id: UserId,
        content: &str,
    ) {
        let _ = self.admit_direct_create(
            message_id,
            channel_id,
            LifecycleContext::DirectMessage,
            user_id,
            None,
            content,
            Timestamp::now(),
        );
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "direct Discord lifecycle binding is explicit"
    )]
    pub(crate) fn admit_direct_create(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        actor_id: UserId,
        thread_parent_id: Option<ChannelId>,
        content: &str,
        version: Timestamp,
    ) -> TransitionResult {
        self.insert_create(
            message_id,
            channel_id,
            context,
            actor_id,
            thread_parent_id,
            content,
            version,
            StoredLineage::Direct,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "verified lifecycle binding is checked field by field"
    )]
    pub(crate) fn admit_verified_create(
        &self,
        facts: LifecycleAdmissionFacts,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        webhook_id: WebhookId,
        actor_id: UserId,
        thread_parent_id: Option<ChannelId>,
        content: &str,
        version: Timestamp,
    ) -> TransitionResult {
        if facts.message_id() != message_id
            || facts.channel_id() != channel_id
            || facts.context() != context
            || facts.webhook_id() != webhook_id
        {
            return TransitionResult::Rejected;
        }
        self.insert_create(
            message_id,
            channel_id,
            context,
            actor_id,
            thread_parent_id,
            content,
            version,
            StoredLineage::Verified(facts),
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "single internal constructor preserves exact lifecycle inputs"
    )]
    fn insert_create(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        actor_id: UserId,
        thread_parent_id: Option<ChannelId>,
        content: &str,
        version: Timestamp,
        lineage: StoredLineage,
    ) -> TransitionResult {
        let now = self.clock.now();
        let Ok(mut state) = self.state.lock() else {
            return TransitionResult::Unavailable;
        };
        self.cleanup(&mut state, now);
        if let Some(tombstone) = state.tombstones.get(&message_id) {
            if tombstone.binding_matches(channel_id, context) || tombstone.lineage_confirmed {
                return TransitionResult::Rejected;
            }
            state.tombstones.remove(&message_id);
        }
        let content_hash = Self::hash_content(content);
        if let Some(existing) = state.active.get(&message_id) {
            return if existing.channel_id == channel_id
                && existing.context == context
                && existing.actor_id == actor_id
                && existing.content_hash == content_hash
                && existing.last_version == version
                && existing.lineage.same_actor_lineage(&lineage)
            {
                TransitionResult::Duplicate
            } else {
                TransitionResult::Rejected
            };
        }
        let Some(next_admission_order) = state.next_admission_order.checked_add(1) else {
            return TransitionResult::Unavailable;
        };
        let admission_order = state.next_admission_order;
        state.next_admission_order = next_admission_order;
        if state.active.len() >= self.max_active
            && let Some(oldest) = state
                .active
                .iter()
                .min_by_key(|(_, record)| record.admission_order)
                .map(|(id, _)| *id)
            && let Some(record) = state.active.remove(&oldest)
        {
            self.insert_tombstone(
                &mut state,
                oldest,
                record.channel_id,
                record.context,
                true,
                now,
                now,
            );
        }
        let record = ActiveRecord {
            channel_id,
            context,
            actor_id,
            thread_parent_id,
            content_hash,
            last_version: version,
            admission_order,
            expires_at: now + self.active_retention,
            lineage,
        };
        let snapshot = Self::snapshot(message_id, &record);
        state.active.insert(message_id, record);
        TransitionResult::Admitted(snapshot)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "verified edit transition checks every bound coordinate"
    )]
    pub(crate) fn transition_verified_edit(
        &self,
        facts: LifecycleAdmissionFacts,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        webhook_id: WebhookId,
        actor_id: UserId,
        content: &str,
        version: Timestamp,
    ) -> TransitionResult {
        if facts.message_id() != message_id
            || facts.channel_id() != channel_id
            || facts.context() != context
            || facts.webhook_id() != webhook_id
        {
            return TransitionResult::Rejected;
        }
        let now = self.clock.now();
        let Ok(mut state) = self.state.lock() else {
            return TransitionResult::Unavailable;
        };
        self.cleanup(&mut state, now);
        let Some(record) = state.active.get_mut(&message_id) else {
            return TransitionResult::Rejected;
        };
        let StoredLineage::Verified(existing) = &record.lineage else {
            return TransitionResult::Rejected;
        };
        if record.channel_id != channel_id
            || record.context != context
            || record.actor_id != actor_id
            || !existing.same_actor_lineage(&facts)
        {
            return TransitionResult::Rejected;
        }
        let content_hash = Self::hash_content(content);
        if version < record.last_version {
            return TransitionResult::Rejected;
        }
        if version == record.last_version {
            return if content_hash == record.content_hash {
                TransitionResult::Duplicate
            } else {
                TransitionResult::Rejected
            };
        }
        if content_hash == record.content_hash {
            record.last_version = version;
            return TransitionResult::Duplicate;
        }
        record.last_version = version;
        record.content_hash = content_hash;
        record.lineage = StoredLineage::Verified(facts);
        TransitionResult::Admitted(Self::snapshot(message_id, record))
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "passive edit transition checks current policy and binding"
    )]
    pub(crate) fn transition_passive_edit(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        actor_id: UserId,
        content: &str,
        version: Timestamp,
        policy_allows: impl FnOnce(&LifecycleView<'_>) -> bool,
    ) -> TransitionResult {
        let now = self.clock.now();
        let Ok(mut state) = self.state.lock() else {
            return TransitionResult::Unavailable;
        };
        self.cleanup(&mut state, now);
        let Some(record) = state.active.get_mut(&message_id) else {
            return TransitionResult::Rejected;
        };
        if record.channel_id != channel_id
            || record.context != context
            || record.actor_id != actor_id
            || !policy_allows(&LifecycleView { record })
        {
            return TransitionResult::Rejected;
        }
        let content_hash = Self::hash_content(content);
        if version < record.last_version {
            return TransitionResult::Rejected;
        }
        if version == record.last_version {
            return if content_hash == record.content_hash {
                TransitionResult::Duplicate
            } else {
                TransitionResult::Rejected
            };
        }
        if content_hash == record.content_hash {
            record.last_version = version;
            return TransitionResult::Duplicate;
        }
        record.last_version = version;
        record.content_hash = content_hash;
        TransitionResult::Admitted(Self::snapshot(message_id, record))
    }

    pub(crate) fn transition_delete(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
    ) -> TransitionResult {
        let now = self.clock.now();
        let Ok(mut state) = self.state.lock() else {
            return TransitionResult::Unavailable;
        };
        self.cleanup(&mut state, now);
        let Some(record) = state.active.get(&message_id) else {
            if let Some(tombstone) = state.tombstones.get(&message_id) {
                return if tombstone.binding_matches(channel_id, context) {
                    TransitionResult::Duplicate
                } else {
                    TransitionResult::Rejected
                };
            }
            self.insert_tombstone(&mut state, message_id, channel_id, context, false, now, now);
            return TransitionResult::Rejected;
        };
        if record.channel_id != channel_id || record.context != context {
            return TransitionResult::Rejected;
        }
        let Some(record) = state.active.remove(&message_id) else {
            return TransitionResult::Unavailable;
        };
        let snapshot = Self::snapshot(message_id, &record);
        self.insert_tombstone(
            &mut state,
            message_id,
            record.channel_id,
            record.context,
            true,
            now,
            now,
        );
        TransitionResult::Admitted(snapshot)
    }

    pub fn verify(&self, message_id: MessageId, claimed_channel: ChannelId) -> VerifyResult {
        let now = self.clock.now();
        let result = match self.state.lock() {
            Ok(mut state) => {
                if state
                    .active
                    .get(&message_id)
                    .is_some_and(|record| record.expires_at <= now)
                {
                    if let Some(record) = state.active.remove(&message_id) {
                        self.insert_tombstone(
                            &mut state,
                            message_id,
                            record.channel_id,
                            record.context,
                            true,
                            record.expires_at,
                            now,
                        );
                    }
                    VerifyResult::Expired
                } else {
                    match state.active.get(&message_id) {
                        None => VerifyResult::Unknown,
                        Some(record) if record.channel_id == claimed_channel => {
                            VerifyResult::Admitted {
                                channel: ChannelRef::new(record.channel_id.get()),
                            }
                        }
                        Some(record) => VerifyResult::ChannelMismatch {
                            admitted_channel: ChannelRef::new(record.channel_id.get()),
                            claimed_channel: ChannelRef::new(claimed_channel.get()),
                        },
                    }
                }
            }
            Err(_) => VerifyResult::Unavailable,
        };
        #[cfg(test)]
        if let Ok(mut observations) = self.observed_verifications.lock() {
            observations.push(result.clone());
        }
        result
    }

    /// Returns an exact-bound active delivery snapshot for trusted reply
    /// identity normalization. Unknown, expired, or mismatched records fail
    /// closed without exposing transport author identity.
    pub(crate) fn active_snapshot(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
    ) -> Option<LifecycleSnapshot> {
        let now = self.clock.now();
        let mut state = self.state.lock().ok()?;
        self.cleanup(&mut state, now);
        let record = state.active.get(&message_id)?;
        (record.channel_id == channel_id && record.context == context)
            .then(|| Self::snapshot(message_id, record))
    }

    pub fn gc_expired(&self) {
        let now = self.clock.now();
        if let Ok(mut state) = self.state.lock() {
            self.cleanup(&mut state, now);
        }
    }

    fn cleanup(&self, state: &mut LedgerState, now: Instant) {
        let expired: Vec<_> = state
            .active
            .iter()
            .filter_map(|(id, record)| {
                (record.expires_at <= now).then_some((
                    *id,
                    record.channel_id,
                    record.context,
                    record.expires_at,
                ))
            })
            .collect();
        for (message_id, channel_id, context, terminal_at) in expired {
            state.active.remove(&message_id);
            self.insert_tombstone(
                state,
                message_id,
                channel_id,
                context,
                true,
                terminal_at,
                now,
            );
        }
        state
            .tombstones
            .retain(|_, tombstone| tombstone.expires_at > now);
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "tombstones retain exact terminal binding and timing"
    )]
    fn insert_tombstone(
        &self,
        state: &mut LedgerState,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
        lineage_confirmed: bool,
        terminal_at: Instant,
        now: Instant,
    ) {
        let expires_at = terminal_at + self.tombstone_retention;
        if expires_at <= now {
            return;
        }
        if state.tombstones.len() >= self.max_tombstones
            && let Some(oldest) = state
                .tombstones
                .iter()
                .min_by_key(|(_, tombstone)| tombstone.terminal_at)
                .map(|(id, _)| *id)
        {
            state.tombstones.remove(&oldest);
        }
        state.tombstones.insert(
            message_id,
            Tombstone {
                channel_id,
                context,
                lineage_confirmed,
                terminal_at,
                expires_at,
            },
        );
    }

    fn snapshot(message_id: MessageId, record: &ActiveRecord) -> LifecycleSnapshot {
        let provenance = match &record.lineage {
            StoredLineage::Direct => None,
            StoredLineage::Verified(facts) => Some(facts.provenance().clone()),
        };
        LifecycleSnapshot {
            channel_id: record.channel_id,
            message_id,
            context: record.context,
            actor_id: record.actor_id,
            thread_parent_id: record.thread_parent_id,
            provenance,
        }
    }

    #[cfg(test)]
    pub fn take_observed_verifications(&self) -> Vec<VerifyResult> {
        self.observed_verifications
            .lock()
            .map(|mut observations| std::mem::take(&mut *observations))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::verified_action::{
        LifecycleProvenance, ResolutionFailureClass, test_lifecycle_facts,
    };
    use proptest::prelude::*;
    use serenity::model::id::GuildId;
    use uuid::Uuid;

    #[derive(Clone)]
    struct ManualClock(Arc<Mutex<Instant>>);

    impl ManualClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Instant::now())))
        }
        fn advance(&self, duration: Duration) {
            if let Ok(mut now) = self.0.lock() {
                *now += duration;
            }
        }
    }

    impl Clock for ManualClock {
        fn now(&self) -> Instant {
            self.0.lock().map_or_else(|e| **e.get_ref(), |now| *now)
        }
    }

    fn ledger(clock: ManualClock, max_active: usize) -> IngressLedger {
        let epoch = clock.now();
        IngressLedger {
            state: Mutex::new(LedgerState::default()),
            clock: Arc::new(clock),
            active_retention: ACTIVE_RETENTION,
            tombstone_retention: TOMBSTONE_RETENTION,
            max_active,
            max_tombstones: 2,
            epoch,
            observed_verifications: Mutex::new(Vec::new()),
        }
    }

    fn admit_direct(ledger: &IngressLedger, id: u64, content: &str) -> TransitionResult {
        ledger.admit_direct_create(
            MessageId::new(id),
            ChannelId::new(100),
            LifecycleContext::Guild(GuildId::new(200)),
            UserId::new(300),
            None,
            content,
            Timestamp::from_unix_timestamp(id as i64).unwrap(),
        )
    }

    fn represented_facts(message_id: u64, user_id: u64) -> LifecycleAdmissionFacts {
        test_lifecycle_facts(
            MessageId::new(message_id),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(400),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(user_id),
                system_id: Some(Uuid::from_u128(user_id as u128)),
                member_id: Some(Uuid::from_u128(user_id as u128 + 1)),
            },
        )
    }

    fn admit_verified(
        ledger: &IngressLedger,
        facts: LifecycleAdmissionFacts,
        content: &str,
        version: i64,
    ) -> TransitionResult {
        ledger.admit_verified_create(
            facts,
            MessageId::new(10),
            ChannelId::new(100),
            LifecycleContext::Guild(GuildId::new(200)),
            WebhookId::new(400),
            UserId::new(500),
            Some(ChannelId::new(99)),
            content,
            Timestamp::from_unix_timestamp(version).unwrap(),
        )
    }

    #[test]
    fn effective_participant_collapses_represented_transport_identity() {
        let ledger = IngressLedger::new();
        let TransitionResult::Admitted(represented) =
            admit_verified(&ledger, represented_facts(10, 700), "hello", 10)
        else {
            panic!("represented create must admit");
        };
        assert_eq!(represented.effective_user_id(), UserId::new(700));

        let direct_ledger = IngressLedger::new();
        let TransitionResult::Admitted(direct) = admit_direct(&direct_ledger, 11, "hello") else {
            panic!("direct create must admit");
        };
        assert_eq!(direct.effective_user_id(), UserId::new(300));

        let app_only_ledger = IngressLedger::new();
        let app_only_facts = test_lifecycle_facts(
            MessageId::new(10),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(400),
            LifecycleProvenance::AppOnly,
        );
        let TransitionResult::Admitted(app_only) =
            admit_verified(&app_only_ledger, app_only_facts, "hello", 10)
        else {
            panic!("app-only create must admit");
        };
        assert_eq!(app_only.effective_user_id(), UserId::new(500));
    }

    #[test]
    fn duplicate_conflict_and_out_of_order_transitions_fail_closed() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_direct(&ledger, 10, "one"),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            admit_direct(&ledger, 10, "one"),
            TransitionResult::Duplicate
        );
        assert_eq!(
            admit_direct(&ledger, 10, "other"),
            TransitionResult::Rejected
        );
        let context = LifecycleContext::Guild(GuildId::new(200));
        assert!(matches!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                context,
                UserId::new(300),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
                |_| true,
            ),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                context,
                UserId::new(300),
                "old",
                Timestamp::from_unix_timestamp(9).unwrap(),
                |_| true,
            ),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                context,
                UserId::new(300),
                "two",
                Timestamp::from_unix_timestamp(13).unwrap(),
                |_| true,
            ),
            TransitionResult::Duplicate
        );
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                context,
                UserId::new(300),
                "out of order",
                Timestamp::from_unix_timestamp(12).unwrap(),
                |_| true,
            ),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                context,
                UserId::new(300),
                "same version conflict",
                Timestamp::from_unix_timestamp(13).unwrap(),
                |_| true,
            ),
            TransitionResult::Rejected
        );
        assert!(matches!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), context),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), context),
            TransitionResult::Duplicate
        );
        assert_eq!(
            admit_direct(&ledger, 10, "late"),
            TransitionResult::Rejected
        );
    }

    #[test]
    fn active_retention_is_seven_days_and_capacity_evicts_oldest() {
        let clock = ManualClock::new();
        let ledger = ledger(clock.clone(), 2);
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        clock.advance(Duration::from_secs(1));
        assert!(matches!(
            admit_direct(&ledger, 11, "b"),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            admit_direct(&ledger, 12, "c"),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.verify(MessageId::new(10), ChannelId::new(100)),
            VerifyResult::Unknown
        );
        assert_eq!(
            admit_direct(&ledger, 10, "late"),
            TransitionResult::Rejected
        );
        assert!(matches!(
            admit_direct(&ledger, 13, "d"),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.verify(MessageId::new(11), ChannelId::new(100)),
            VerifyResult::Unknown
        );
        assert!(matches!(
            ledger.verify(MessageId::new(12), ChannelId::new(100)),
            VerifyResult::Admitted { .. }
        ));
        clock.advance(ACTIVE_RETENTION);
        assert_eq!(
            ledger.verify(MessageId::new(12), ChannelId::new(100)),
            VerifyResult::Expired
        );
    }

    #[test]
    fn tombstone_retention_is_bounded_to_one_day() {
        let clock = ManualClock::new();
        let ledger = ledger(clock.clone(), 2);
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        let context = LifecycleContext::Guild(GuildId::new(200));
        assert!(matches!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), context),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            admit_direct(&ledger, 10, "replay"),
            TransitionResult::Rejected
        );
        clock.advance(TOMBSTONE_RETENTION);
        assert!(matches!(
            admit_direct(&ledger, 10, "new after bounded retention"),
            TransitionResult::Admitted(_)
        ));
    }

    #[test]
    fn lazy_cleanup_does_not_refresh_expiry_tombstone_retention() {
        let clock = ManualClock::new();
        let ledger = ledger(clock.clone(), 2);
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        clock.advance(ACTIVE_RETENTION + TOMBSTONE_RETENTION);
        ledger.gc_expired();
        assert!(matches!(
            admit_direct(&ledger, 10, "new after complete lifecycle window"),
            TransitionResult::Admitted(_)
        ));
    }

    #[test]
    fn tombstone_binding_mismatch_is_rejected_without_suppressing_later_create() {
        let ledger = IngressLedger::new();
        let guild = LifecycleContext::Guild(GuildId::new(200));
        assert_eq!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(999), guild),
            TransitionResult::Rejected
        );
        assert!(matches!(
            admit_direct(&ledger, 10, "canonical create"),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), guild),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.admit_direct_create(
                MessageId::new(10),
                ChannelId::new(999),
                guild,
                UserId::new(300),
                None,
                "conflicting create",
                Timestamp::from_unix_timestamp(20).unwrap(),
            ),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(999), guild),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), guild),
            TransitionResult::Duplicate
        );
    }

    #[test]
    fn admission_order_exhaustion_fails_without_evicting_existing_lineage() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        ledger
            .state
            .lock()
            .expect("test ledger lock")
            .next_admission_order = u64::MAX;
        assert_eq!(
            admit_direct(&ledger, 11, "b"),
            TransitionResult::Unavailable
        );
        assert!(matches!(
            ledger.verify(MessageId::new(10), ChannelId::new(100)),
            VerifyResult::Admitted { .. }
        ));
    }

    #[test]
    fn active_admission_survives_five_minutes_without_refresh() {
        let clock = ManualClock::new();
        let ledger = ledger(clock.clone(), 2);
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        clock.advance(Duration::from_secs(5 * 60 + 1));
        assert!(matches!(
            ledger.verify(MessageId::new(10), ChannelId::new(100)),
            VerifyResult::Admitted { .. }
        ));
    }

    #[test]
    fn verified_actor_drift_and_transient_unavailable_cannot_rewrite_lineage() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_verified(&ledger, represented_facts(10, 600), "one", 10),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            admit_verified(&ledger, represented_facts(10, 601), "one", 10),
            TransitionResult::Rejected
        );
        let webhook_drift = test_lifecycle_facts(
            MessageId::new(10),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(401),
            LifecycleProvenance::Represented {
                discord_user_id: UserId::new(600),
                system_id: Some(Uuid::from_u128(600)),
                member_id: Some(Uuid::from_u128(601)),
            },
        );
        assert_eq!(
            ledger.transition_verified_edit(
                webhook_drift,
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(401),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
            ),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_verified_edit(
                represented_facts(10, 601),
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
            ),
            TransitionResult::Rejected
        );
        let unavailable = test_lifecycle_facts(
            MessageId::new(10),
            ChannelId::new(100),
            GuildId::new(200),
            WebhookId::new(400),
            LifecycleProvenance::Unavailable(ResolutionFailureClass::Deadline),
        );
        assert_eq!(
            ledger.transition_verified_edit(
                unavailable,
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
            ),
            TransitionResult::Rejected
        );
        assert!(matches!(
            ledger.transition_verified_edit(
                represented_facts(10, 600),
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
            ),
            TransitionResult::Admitted(_)
        ));
    }

    #[test]
    fn passive_edit_rechecks_policy_and_exact_binding() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_verified(&ledger, represented_facts(10, 600), "one", 10),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
                |lineage| {
                    matches!(
                        lineage.provenance(),
                        Some(LifecycleProvenance::Represented { discord_user_id, .. })
                            if discord_user_id.get() == 999
                    )
                },
            ),
            TransitionResult::Rejected
        );
        assert_eq!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::DirectMessage,
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
                |_| true,
            ),
            TransitionResult::Rejected
        );
        assert!(matches!(
            ledger.transition_passive_edit(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                UserId::new(500),
                "two",
                Timestamp::from_unix_timestamp(11).unwrap(),
                |lineage| {
                    matches!(
                        lineage.provenance(),
                        Some(LifecycleProvenance::Represented { discord_user_id, .. })
                            if discord_user_id.get() == 600
                    )
                },
            ),
            TransitionResult::Admitted(_)
        ));
    }

    #[test]
    fn delete_batch_is_per_id_atomic_and_idempotent() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            admit_direct(&ledger, 11, "b"),
            TransitionResult::Admitted(_)
        ));
        let context = LifecycleContext::Guild(GuildId::new(200));
        let first: Vec<_> = [10, 99, 11]
            .into_iter()
            .map(|id| ledger.transition_delete(MessageId::new(id), ChannelId::new(100), context))
            .collect();
        assert!(matches!(first[0], TransitionResult::Admitted(_)));
        assert_eq!(first[1], TransitionResult::Rejected);
        assert!(matches!(first[2], TransitionResult::Admitted(_)));
        assert_eq!(
            ledger.transition_delete(MessageId::new(10), ChannelId::new(100), context),
            TransitionResult::Duplicate
        );
        assert_eq!(
            admit_direct(&ledger, 99, "late create"),
            TransitionResult::Rejected
        );
    }

    #[test]
    fn distinct_original_and_repost_ids_never_correlate() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_verified(
                &ledger,
                test_lifecycle_facts(
                    MessageId::new(10),
                    ChannelId::new(100),
                    GuildId::new(200),
                    WebhookId::new(400),
                    LifecycleProvenance::AppOnly,
                ),
                "original",
                10,
            ),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            ledger.admit_verified_create(
                test_lifecycle_facts(
                    MessageId::new(11),
                    ChannelId::new(100),
                    GuildId::new(200),
                    WebhookId::new(400),
                    LifecycleProvenance::AppOnly,
                ),
                MessageId::new(11),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
                WebhookId::new(400),
                UserId::new(500),
                None,
                "repost",
                Timestamp::from_unix_timestamp(11).unwrap(),
            ),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            ledger.transition_delete(
                MessageId::new(10),
                ChannelId::new(100),
                LifecycleContext::Guild(GuildId::new(200)),
            ),
            TransitionResult::Admitted(_)
        ));
        assert!(matches!(
            ledger.verify(MessageId::new(11), ChannelId::new(100)),
            VerifyResult::Admitted { .. }
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn verified_create_rejects_every_single_coordinate_mutation(
            dimension in 0u8..5,
            message in 1u64..u64::MAX,
            channel in 1u64..u64::MAX,
            guild in 1u64..u64::MAX,
            webhook in 1u64..u64::MAX,
        ) {
            let alternate = |value: u64| if value == u64::MAX { 1 } else { value + 1 };
            let facts = test_lifecycle_facts(
                MessageId::new(message),
                ChannelId::new(channel),
                GuildId::new(guild),
                WebhookId::new(webhook),
                LifecycleProvenance::AppOnly,
            );
            let raw_message = MessageId::new(if dimension == 0 { alternate(message) } else { message });
            let raw_channel = ChannelId::new(if dimension == 1 { alternate(channel) } else { channel });
            let raw_context = if dimension == 3 {
                LifecycleContext::DirectMessage
            } else {
                LifecycleContext::Guild(GuildId::new(
                    if dimension == 2 { alternate(guild) } else { guild },
                ))
            };
            let raw_webhook = WebhookId::new(if dimension == 4 { alternate(webhook) } else { webhook });

            prop_assert!(matches!(
                IngressLedger::new().admit_verified_create(
                    facts,
                    raw_message,
                    raw_channel,
                    raw_context,
                    raw_webhook,
                    UserId::new(500),
                    None,
                    "content",
                    Timestamp::from_unix_timestamp(1).unwrap(),
                ),
                TransitionResult::Rejected
            ));
        }

        #[test]
        fn arbitrary_lifecycle_sequences_never_admit_passive_without_matching_active(
            operations in proptest::collection::vec(
                (0u8..5, 1u64..8, any::<bool>(), any::<bool>(), 1i64..40, any::<bool>()),
                1..100,
            )
        ) {
            let clock = ManualClock::new();
            let ledger = ledger(clock.clone(), 3);
            for (kind, id, alternate_channel, dm, version, alternate_content) in operations {
                let channel = ChannelId::new(if alternate_channel { 101 } else { 100 });
                let context = if dm {
                    LifecycleContext::DirectMessage
                } else {
                    LifecycleContext::Guild(GuildId::new(200))
                };
                let content = if alternate_content { "alternate" } else { "content" };
                match kind {
                    0 => {
                        let _ = ledger.admit_direct_create(
                            MessageId::new(id), channel, context, UserId::new(300), None, content,
                            Timestamp::from_unix_timestamp(version).unwrap(),
                        );
                    }
                    1 | 2 => {
                        let had_matching_active = ledger.state.lock().is_ok_and(|state| {
                            state.active.get(&MessageId::new(id)).is_some_and(|record| {
                                record.channel_id == channel
                                    && record.context == context
                                    && record.actor_id == UserId::new(300)
                            })
                        });
                        let result = ledger.transition_passive_edit(
                            MessageId::new(id), channel, context, UserId::new(300), content,
                            Timestamp::from_unix_timestamp(version).unwrap(), |_| true,
                        );
                        if matches!(result, TransitionResult::Admitted(_)) {
                            prop_assert!(had_matching_active);
                        }
                    }
                    3 => {
                        let had_matching_active = ledger.state.lock().is_ok_and(|state| {
                            state.active.get(&MessageId::new(id)).is_some_and(|record| {
                                record.channel_id == channel && record.context == context
                            })
                        });
                        let result = ledger.transition_delete(MessageId::new(id), channel, context);
                        if matches!(result, TransitionResult::Admitted(_)) {
                            prop_assert!(had_matching_active);
                            let was_removed = ledger.state.lock().is_ok_and(|state| {
                                !state.active.contains_key(&MessageId::new(id))
                            });
                            prop_assert!(was_removed);
                        }
                    }
                    _ => {
                        clock.advance(if alternate_content {
                            ACTIVE_RETENTION + TOMBSTONE_RETENTION
                        } else {
                            Duration::from_secs(5 * 60 + 1)
                        });
                        ledger.gc_expired();
                    }
                }
                let bounds_hold = ledger.state.lock().is_ok_and(|state| {
                    state.active.len() <= 3
                        && state.tombstones.len() <= 2
                        && state.active.keys().all(|id| !state.tombstones.contains_key(id))
                });
                prop_assert!(bounds_hold);
            }
        }
    }
}
