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

#[cfg(test)]
use dione_discord_events::lifecycle_storage::{ACTIVE_RETENTION, TOMBSTONE_RETENTION};
use dione_discord_events::lifecycle_storage::{
    CreateEntry, DeleteOutcome, EditOutcome, LifecycleStorage, Lookup, PayloadChange, RecordView,
    StorageErrorKind, StorageLimits, Verification,
};
use dione_discord_messaging_contracts::ChannelRef;
use serenity::model::{
    Timestamp,
    id::{ChannelId, MessageId, UserId, WebhookId},
};
#[cfg(test)]
use std::sync::Mutex;
use std::{sync::Arc, time::Instant};
use {
    crate::discord::verified_action::{
        LifecycleAdmissionFacts, LifecycleContext, LifecycleProvenance,
    },
    dione_attention::types::{DirectAuthorKind, SourceAuthorKind},
};

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
    Direct(DirectAuthorKind),
    Verified(LifecycleAdmissionFacts),
}

impl StoredLineage {
    fn same_actor_lineage(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Direct(left), Self::Direct(right)) => left == right,
            (Self::Verified(left), Self::Verified(right)) => left.same_actor_lineage(right),
            (Self::Direct(_), Self::Verified(_)) | (Self::Verified(_), Self::Direct(_)) => false,
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
    author_kind: SourceAuthorKind,
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

    /// Returns the verified transport-aware author class captured at admission.
    pub(crate) fn author_kind(&self) -> SourceAuthorKind {
        self.author_kind
    }

    pub(crate) fn provenance(&self) -> Option<&LifecycleProvenance> {
        self.provenance.as_ref()
    }
}

/// Borrowed lineage supplied to pure current-policy edit checks.
pub(crate) struct LifecycleView<'a> {
    record: RecordView<'a, LifecycleContext, StoredLineage>,
}

impl LifecycleView<'_> {
    pub(crate) fn actor_id(&self) -> UserId {
        self.record.actor_id()
    }
    pub(crate) fn provenance(&self) -> Option<&LifecycleProvenance> {
        match self.record.payload() {
            StoredLineage::Direct(_) => None,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceDeliveryFailure {
    Unavailable,
    Invalidated,
}

pub struct IngressLedger {
    records: LifecycleStorage<LifecycleContext, StoredLineage>,
    clock: Arc<dyn Clock>,
    epoch: Instant,
    #[cfg(test)]
    observed_verifications: Mutex<Vec<VerifyResult>>,
}

impl Default for IngressLedger {
    fn default() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let epoch = clock.now();
        Self {
            records: LifecycleStorage::new(StorageLimits::default()),
            clock,
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
            DirectAuthorKind::Human,
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
        author_kind: DirectAuthorKind,
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
            StoredLineage::Direct(author_kind),
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
        self.records
            .with_locked(|locked| {
                let mut transaction = locked.transitions(now);
                match transaction.create_entry(message_id, channel_id, context, content, version) {
                    CreateEntry::Blocked => TransitionResult::Rejected,
                    CreateEntry::Existing {
                        record,
                        content_and_version_match,
                    } => {
                        if record.channel_id() == channel_id
                            && record.context() == context
                            && record.actor_id() == actor_id
                            && content_and_version_match
                            && record.payload().same_actor_lineage(&lineage)
                        {
                            TransitionResult::Duplicate
                        } else {
                            TransitionResult::Rejected
                        }
                    }
                    CreateEntry::Vacant(slot) => {
                        match slot.insert(actor_id, thread_parent_id, lineage) {
                            Ok(record) => {
                                TransitionResult::Admitted(Self::snapshot(message_id, record))
                            }
                            Err(error) => Self::storage_failure(error.kind()),
                        }
                    }
                }
            })
            .unwrap_or(TransitionResult::Unavailable)
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
        self.records
            .with_locked(|locked| {
                let mut transaction = locked.transitions(now);
                let Some(entry) = transaction.edit_entry(message_id) else {
                    return TransitionResult::Rejected;
                };
                {
                    let record = entry.record();
                    let StoredLineage::Verified(existing) = record.payload() else {
                        return TransitionResult::Rejected;
                    };
                    if record.channel_id() != channel_id
                        || record.context() != context
                        || record.actor_id() != actor_id
                        || !existing.same_actor_lineage(&facts)
                    {
                        return TransitionResult::Rejected;
                    }
                }
                match entry.apply(
                    content,
                    version,
                    PayloadChange::Replace(StoredLineage::Verified(facts)),
                ) {
                    EditOutcome::Rejected => TransitionResult::Rejected,
                    EditOutcome::Duplicate => TransitionResult::Duplicate,
                    EditOutcome::Changed(record) => {
                        TransitionResult::Admitted(Self::snapshot(message_id, record))
                    }
                }
            })
            .unwrap_or(TransitionResult::Unavailable)
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
        self.records
            .with_locked(|locked| {
                let mut transaction = locked.transitions(now);
                let Some(entry) = transaction.edit_entry(message_id) else {
                    return TransitionResult::Rejected;
                };
                {
                    let record = entry.record();
                    if record.channel_id() != channel_id
                        || record.context() != context
                        || record.actor_id() != actor_id
                        || !policy_allows(&LifecycleView { record })
                    {
                        return TransitionResult::Rejected;
                    }
                }
                match entry.apply(content, version, PayloadChange::Keep) {
                    EditOutcome::Rejected => TransitionResult::Rejected,
                    EditOutcome::Duplicate => TransitionResult::Duplicate,
                    EditOutcome::Changed(record) => {
                        TransitionResult::Admitted(Self::snapshot(message_id, record))
                    }
                }
            })
            .unwrap_or(TransitionResult::Unavailable)
    }

    pub(crate) fn transition_delete(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        context: LifecycleContext,
    ) -> TransitionResult {
        let now = self.clock.now();
        self.records
            .with_locked(|locked| {
                let mut transaction = locked.transitions(now);
                match transaction.delete(message_id, channel_id, context) {
                    DeleteOutcome::Rejected => TransitionResult::Rejected,
                    DeleteOutcome::Duplicate => TransitionResult::Duplicate,
                    DeleteOutcome::Unavailable => TransitionResult::Unavailable,
                    DeleteOutcome::Removed(record) => {
                        TransitionResult::Admitted(Self::snapshot(message_id, record.view()))
                    }
                }
            })
            .unwrap_or(TransitionResult::Unavailable)
    }

    pub fn verify(&self, message_id: MessageId, claimed_channel: ChannelId) -> VerifyResult {
        let now = self.clock.now();
        let result = self
            .records
            .with_locked(
                |locked| match locked.verify_target(now, message_id, claimed_channel) {
                    Verification::Unknown => VerifyResult::Unknown,
                    Verification::Expired => VerifyResult::Expired,
                    Verification::Found(channel) => VerifyResult::Admitted {
                        channel: ChannelRef::new(channel.get()),
                    },
                    Verification::ChannelMismatch(channel) => VerifyResult::ChannelMismatch {
                        admitted_channel: ChannelRef::new(channel.get()),
                        claimed_channel: ChannelRef::new(claimed_channel.get()),
                    },
                },
            )
            .unwrap_or(VerifyResult::Unavailable);
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
        self.records
            .with_locked(|locked| {
                let transaction = locked.transitions(now);
                match transaction.lookup(message_id) {
                    Lookup::Active(record) => (record.channel_id() == channel_id
                        && record.context() == context)
                        .then(|| Self::snapshot(message_id, record)),
                    Lookup::Missing | Lookup::Terminal => None,
                }
            })
            .ok()
            .flatten()
    }

    /// Recheck a source version and its existing admitted lineage at a delayed effect.
    /// Unknown/expired lineage is not permission; callers must re-resolve it.
    pub(crate) fn delivery_check(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        expected_hash: &str,
        config: &dione_config::LoadedConfig,
        targeting: dione_discord_events::MessageTargeting,
    ) -> Result<(), SourceDeliveryFailure> {
        self.records
            .with_locked(|locked| {
                let transaction = locked.transitions(self.clock.now());
                let record = match transaction.lookup(message_id) {
                    Lookup::Terminal => return Err(SourceDeliveryFailure::Invalidated),
                    Lookup::Missing => return Err(SourceDeliveryFailure::Unavailable),
                    Lookup::Active(record) => record,
                };
                if record.channel_id() != channel_id || !record.matches_hash_hex(expected_hash) {
                    return Err(SourceDeliveryFailure::Invalidated);
                }
                if matches!(
                    record.payload(),
                    StoredLineage::Direct(DirectAuthorKind::Bot)
                ) && !config.is_allowed(record.actor_id().get())
                {
                    return Err(SourceDeliveryFailure::Invalidated);
                }
                let context = record.context();
                let actor_id = record.actor_id();
                let thread_parent_id = record.thread_parent_id();
                let lineage = LifecycleView { record };
                match context {
                    LifecycleContext::DirectMessage => matches!(
                        dione_discord_gate::InboundGate::check_dm(config, actor_id.get()),
                        dione_discord_gate::GateDecision::Deliver
                    ),
                    LifecycleContext::Guild(guild) => {
                        let scope = if config.channel_policy(channel_id.get()).is_some() {
                            channel_id
                        } else {
                            thread_parent_id.unwrap_or(channel_id)
                        };
                        let mention = match targeting {
                            dione_discord_events::MessageTargeting::GuildDirected(kind) => {
                                Some(kind)
                            }
                            _ => None,
                        };
                        crate::discord::events::passive_edit_policy_allows(
                            config,
                            scope.get(),
                            Some(guild.get()),
                            mention,
                            &lineage,
                        )
                    }
                }
                .then_some(())
                .ok_or(SourceDeliveryFailure::Invalidated)
            })
            .map_err(|_| SourceDeliveryFailure::Unavailable)?
    }

    pub fn gc_expired(&self) {
        let now = self.clock.now();
        let _ = self.records.with_locked(|locked| {
            let _ = locked.transitions(now);
        });
    }

    fn storage_failure(kind: &StorageErrorKind) -> TransitionResult {
        match kind {
            StorageErrorKind::LockPoisoned | StorageErrorKind::SequenceExhausted => {
                TransitionResult::Unavailable
            }
        }
    }

    fn snapshot(
        message_id: MessageId,
        record: RecordView<'_, LifecycleContext, StoredLineage>,
    ) -> LifecycleSnapshot {
        let (author_kind, provenance) = match record.payload() {
            StoredLineage::Direct(kind) => ((*kind).into(), None),
            StoredLineage::Verified(facts) => (
                SourceAuthorKind::VerifiedWebhook,
                Some(facts.provenance().clone()),
            ),
        };
        LifecycleSnapshot {
            channel_id: record.channel_id(),
            message_id,
            context: record.context(),
            actor_id: record.actor_id(),
            author_kind,
            thread_parent_id: record.thread_parent_id(),
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
    use std::time::Duration;
    use uuid::Uuid;

    #[test]
    fn channel_ref_preserves_and_displays_the_channel_id() {
        let channel = ChannelRef::new(42);

        assert_eq!(channel.get(), 42);
        assert_eq!(channel.to_string(), "42");
    }

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
            records: LifecycleStorage::new(StorageLimits::default().with_capacity(max_active, 2)),
            clock: Arc::new(clock),
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
            DirectAuthorKind::Human,
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
                DirectAuthorKind::Human,
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
    fn storage_failures_translate_to_unavailable_without_rewriting_lineage() {
        let ledger = IngressLedger::new();
        assert!(matches!(
            admit_direct(&ledger, 10, "a"),
            TransitionResult::Admitted(_)
        ));
        assert_eq!(
            IngressLedger::storage_failure(&StorageErrorKind::SequenceExhausted),
            TransitionResult::Unavailable
        );
        assert_eq!(
            IngressLedger::storage_failure(&StorageErrorKind::LockPoisoned),
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
                            MessageId::new(id), channel, context, UserId::new(300),
                            DirectAuthorKind::Human, None, content,
                            Timestamp::from_unix_timestamp(version).unwrap(),
                        );
                    }
                    1 | 2 => {
                        let had_matching_active = ledger.records.with_locked(|locked| {
                            locked.inspect_retained(MessageId::new(id)).is_some_and(|record| {
                                record.channel_id() == channel
                                    && record.context() == context
                                    && record.actor_id() == UserId::new(300)
                            })
                        }).unwrap_or(false);
                        let result = ledger.transition_passive_edit(
                            MessageId::new(id), channel, context, UserId::new(300), content,
                            Timestamp::from_unix_timestamp(version).unwrap(), |_| true,
                        );
                        if matches!(result, TransitionResult::Admitted(_)) {
                            prop_assert!(had_matching_active);
                        }
                    }
                    3 => {
                        let had_matching_active = ledger.records.with_locked(|locked| {
                            locked.inspect_retained(MessageId::new(id)).is_some_and(|record| {
                                record.channel_id() == channel && record.context() == context
                            })
                        }).unwrap_or(false);
                        let result = ledger.transition_delete(MessageId::new(id), channel, context);
                        if matches!(result, TransitionResult::Admitted(_)) {
                            prop_assert!(had_matching_active);
                            let was_removed = ledger.records.with_locked(|locked| {
                                locked.inspect_retained(MessageId::new(id)).is_none()
                            }).unwrap_or(false);
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

            }
        }
    }
}
