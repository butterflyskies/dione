//! Linear proof types for verified Discord app actions.
//!
//! This module owns the construction and state-transition boundary. Runtime
//! verification and resolution live in the sibling adapter module; handlers
//! consume these linear actions without reconstructing their proof fields.

use chrono::{DateTime, Utc};
use serenity::model::{
    channel::Message,
    id::{ChannelId, GuildId, MessageId, UserId, WebhookId},
};
use std::collections::HashSet;
use uuid::Uuid;

const PLURALKIT_APPLICATION_ID: u64 = 466_378_653_216_014_359;

/// Whether the bound event occurred in a guild or a direct message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EventContext {
    /// The event occurred in the identified guild.
    Guild(GuildId),
    /// The event occurred in a direct-message channel.
    DirectMessage,
}

/// The complete immutable coordinates of one Discord webhook event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct EventBinding {
    message_id: MessageId,
    channel_id: ChannelId,
    context: EventContext,
    webhook_id: WebhookId,
}

impl EventBinding {
    pub(super) fn from_verified_update(
        message_id: MessageId,
        channel_id: ChannelId,
        context: EventContext,
        webhook_id: WebhookId,
    ) -> Option<Self> {
        if message_id.get() == 0
            || channel_id.get() == 0
            || webhook_id.get() == 0
            || matches!(&context, EventContext::Guild(id) if id.get() == 0)
        {
            return None;
        }
        Some(Self {
            message_id,
            channel_id,
            context,
            webhook_id,
        })
    }

    /// Returns the bound Discord message ID.
    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }

    /// Returns the bound Discord channel ID.
    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    /// Returns the bound guild or direct-message context.
    pub(crate) fn context(&self) -> &EventContext {
        &self.context
    }

    /// Returns the bound Discord webhook ID.
    pub(crate) fn webhook_id(&self) -> WebhookId {
        self.webhook_id
    }
}

/// A supported provider selected from verified Discord creator facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportProvider {
    /// The official PluralKit Discord application.
    PluralKit,
}

impl TransportProvider {
    pub(super) fn from_creator_user_id(creator_user_id: Option<u64>) -> Option<Self> {
        match creator_user_id {
            Some(PLURALKIT_APPLICATION_ID) => Some(Self::PluralKit),
            Some(_) | None => None,
        }
    }
}

/// Linear proof that Discord transport verification succeeded for an event.
#[derive(Debug)]
pub(crate) struct VerifiedTransport {
    provider: TransportProvider,
}

impl VerifiedTransport {
    /// Returns the provider established by transport verification.
    pub(crate) fn provider(&self) -> TransportProvider {
        self.provider
    }
}

/// A represented principal attested by the verified provider.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RepresentedPrincipal {
    discord_user_id: UserId,
    system_id: Option<Uuid>,
    member_id: Option<Uuid>,
}

impl RepresentedPrincipal {
    pub(super) fn from_verified_facts(
        discord_user_id: UserId,
        system_id: Option<Uuid>,
        member_id: Option<Uuid>,
    ) -> Self {
        Self {
            discord_user_id,
            system_id,
            member_id,
        }
    }

    /// Returns the represented Discord user.
    #[cfg(test)]
    pub(crate) fn discord_user_id(&self) -> UserId {
        self.discord_user_id
    }
}

/// A typed reason that represented-principal resolution was unavailable.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResolutionFailure {
    kind: ResolutionFailureKind,
}

#[derive(Debug, PartialEq, Eq)]
enum ResolutionFailureKind {
    Deadline,
    Network,
    InvalidResponse,
    Provider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolutionFailureClass {
    Deadline,
    Network,
    InvalidResponse,
    Provider,
}

impl ResolutionFailure {
    pub(super) fn from_class(class: ResolutionFailureClass) -> Self {
        let kind = match class {
            ResolutionFailureClass::Deadline => ResolutionFailureKind::Deadline,
            ResolutionFailureClass::Network => ResolutionFailureKind::Network,
            ResolutionFailureClass::InvalidResponse => ResolutionFailureKind::InvalidResponse,
            ResolutionFailureClass::Provider => ResolutionFailureKind::Provider,
        };
        Self { kind }
    }

    pub(crate) fn class(&self) -> ResolutionFailureClass {
        match self.kind {
            ResolutionFailureKind::Deadline => ResolutionFailureClass::Deadline,
            ResolutionFailureKind::Network => ResolutionFailureClass::Network,
            ResolutionFailureKind::InvalidResponse => ResolutionFailureClass::InvalidResponse,
            ResolutionFailureKind::Provider => ResolutionFailureClass::Provider,
        }
    }
}

mod state {
    use super::ResolvedState;

    pub(crate) trait Sealed {}

    pub(crate) trait ActionState: Sealed {
        type Payload;
    }

    /// Transport is verified, but principal resolution has not completed.
    #[derive(Debug)]
    pub(crate) struct Unresolved {
        pub(super) private: (),
    }

    /// Principal resolution completed with exactly one sealed outcome.
    #[derive(Debug)]
    pub(crate) struct Resolved {
        pub(super) state: ResolvedState,
    }

    impl Sealed for Unresolved {}
    impl Sealed for Resolved {}

    impl ActionState for Unresolved {
        type Payload = Self;
    }

    impl ActionState for Resolved {
        type Payload = Self;
    }
}

pub(crate) use state::{Resolved, Unresolved};

#[derive(Debug)]
enum ResolvedState {
    AppOnly,
    Represented(RepresentedPrincipal),
    Unavailable(ResolutionFailure),
}

/// One verified app action in a linear typestate.
#[derive(Debug)]
pub(crate) struct VerifiedAppAction<S>
where
    S: state::ActionState<Payload = S>,
{
    transport: VerifiedTransport,
    binding: EventBinding,
    state: S,
}

impl<S> VerifiedAppAction<S>
where
    S: state::ActionState<Payload = S>,
{
    /// Returns the immutable event binding carried by this action.
    pub(crate) fn binding(&self) -> &EventBinding {
        &self.binding
    }

    /// Returns the verified transport provider without exposing the proof.
    pub(crate) fn provider(&self) -> TransportProvider {
        self.transport.provider()
    }
}

/// The sole owner of transport-proof minting.
pub(crate) struct DiscordTransportVerifier {
    // The unread private field prevents sibling modules from constructing the sole
    // proof-minting authority. `allow` rather than `expect`: whether `dead_code`
    // fires here varies by toolchain and compilation context (Rust 1.98 skips it in
    // trybuild's compile-fail builds), and an unfulfilled expectation is itself a
    // warning that breaks the snapshots.
    #[allow(dead_code)]
    private: (),
}

impl DiscordTransportVerifier {
    /// Creates the sole production transport verifier.
    pub(crate) fn new() -> Self {
        Self { private: () }
    }

    /// Mints a proof from one Discord event after provider verification.
    ///
    /// This remains private until the transport-verification atom supplies the
    /// bounded Discord creator lookup that selects `provider`.
    pub(super) fn mint_verified(
        &self,
        event: Message,
        provider: TransportProvider,
    ) -> Option<VerifiedAppAction<Unresolved>> {
        let webhook_id = event.webhook_id?;
        let context = event
            .guild_id
            .map_or(EventContext::DirectMessage, EventContext::Guild);

        Some(VerifiedAppAction {
            transport: VerifiedTransport { provider },
            binding: EventBinding {
                message_id: event.id,
                channel_id: event.channel_id,
                context,
                webhook_id,
            },
            state: Unresolved { private: () },
        })
    }

    pub(super) fn mint_verified_update(
        &self,
        binding: EventBinding,
        provider: TransportProvider,
    ) -> VerifiedAppAction<Unresolved> {
        VerifiedAppAction {
            transport: VerifiedTransport { provider },
            binding,
            state: Unresolved { private: () },
        }
    }
}

/// An owned, unresolved action held across provider resolution.
#[derive(Debug)]
pub(crate) struct ResolutionSession {
    action: VerifiedAppAction<Unresolved>,
}

impl ResolutionSession {
    /// Returns the complete immutable binding selected by verification.
    pub(crate) fn binding(&self) -> &EventBinding {
        self.action.binding()
    }

    /// Returns the exact message key selected by transport verification.
    #[cfg(test)]
    pub(crate) fn message_id(&self) -> MessageId {
        self.binding().message_id()
    }

    /// Returns the verified provider used to select a fixed resolver origin.
    pub(crate) fn provider(&self) -> TransportProvider {
        self.action.provider()
    }
}

/// The sole owner of unresolved-to-resolved typestate transitions.
pub(crate) struct PrincipalResolver {
    // The unread private field prevents sibling modules from constructing the sole
    // typestate transition authority. `allow` rather than `expect`: see
    // `DiscordTransportVerifier::private` — expectation fulfillment is
    // toolchain/context dependent.
    #[allow(dead_code)]
    private: (),
}

impl PrincipalResolver {
    /// Creates the sole typestate transition authority.
    pub(crate) fn new() -> Self {
        Self { private: () }
    }

    /// Takes exclusive ownership of an unresolved action for resolution.
    pub(crate) fn begin(&self, action: VerifiedAppAction<Unresolved>) -> ResolutionSession {
        ResolutionSession { action }
    }

    fn finish(
        &self,
        session: ResolutionSession,
        state: ResolvedState,
    ) -> VerifiedAppAction<Resolved> {
        let VerifiedAppAction {
            transport,
            binding,
            state: Unresolved { private: () },
        } = session.action;

        VerifiedAppAction {
            transport,
            binding,
            state: Resolved { state },
        }
    }

    pub(super) fn finish_app_only(
        &self,
        session: ResolutionSession,
    ) -> VerifiedAppAction<Resolved> {
        self.finish(session, ResolvedState::AppOnly)
    }

    pub(super) fn finish_represented(
        &self,
        session: ResolutionSession,
        principal: RepresentedPrincipal,
    ) -> VerifiedAppAction<Resolved> {
        self.finish(session, ResolvedState::Represented(principal))
    }

    pub(super) fn finish_unavailable(
        &self,
        session: ResolutionSession,
        failure: ResolutionFailure,
    ) -> VerifiedAppAction<Resolved> {
        self.finish(session, ResolvedState::Unavailable(failure))
    }
}

/// A monotonic policy generation attached to one fresh snapshot.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PolicyGeneration(u64);

impl PolicyGeneration {
    /// Returns the generation value.
    #[cfg(test)]
    pub(crate) fn get(&self) -> u64 {
        self.0
    }
}

/// A stable fingerprint of the policy inputs used for admission.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PolicyFingerprint([u8; 32]);

impl PolicyFingerprint {
    /// Returns the fingerprint bytes.
    #[cfg(test)]
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// The identity-relevant part of one freshly derived policy snapshot.
#[derive(Debug)]
pub(crate) struct FreshPolicySnapshot {
    restriction_intended: bool,
    allowed_discord_users: HashSet<UserId>,
    allowed_systems: HashSet<Uuid>,
    allowed_members: HashSet<Uuid>,
    generation: PolicyGeneration,
    fingerprint: PolicyFingerprint,
    evaluated_at: DateTime<Utc>,
}

impl FreshPolicySnapshot {
    pub(super) fn from_parts(
        restriction_intended: bool,
        allowed_discord_users: HashSet<UserId>,
        allowed_systems: HashSet<Uuid>,
        allowed_members: HashSet<Uuid>,
        generation: u64,
        fingerprint: [u8; 32],
        evaluated_at: DateTime<Utc>,
    ) -> Self {
        Self {
            restriction_intended,
            allowed_discord_users,
            allowed_systems,
            allowed_members,
            generation: PolicyGeneration(generation),
            fingerprint: PolicyFingerprint(fingerprint),
            evaluated_at,
        }
    }

    fn has_identity_filter(&self) -> bool {
        self.restriction_intended
            || !self.allowed_discord_users.is_empty()
            || !self.allowed_systems.is_empty()
            || !self.allowed_members.is_empty()
    }

    #[cfg(test)]
    pub(super) fn restriction_intended(&self) -> bool {
        self.restriction_intended
    }

    #[cfg(test)]
    pub(super) fn fingerprint_bytes(&self) -> &[u8; 32] {
        self.fingerprint.as_bytes()
    }

    fn admits(&self, principal: &RepresentedPrincipal) -> bool {
        self.allowed_discord_users
            .contains(&principal.discord_user_id)
            || principal
                .system_id
                .is_some_and(|id| self.allowed_systems.contains(&id))
            || principal
                .member_id
                .is_some_and(|id| self.allowed_members.contains(&id))
    }
}

/// The consuming verdict returned by the verified-action gate.
#[derive(Debug)]
pub(crate) enum VerifiedGateVerdict {
    /// The action was allowed and yielded durable admission facts.
    Allow(AdmissionFacts),
    /// The action was denied and its linear proof was destroyed.
    Deny,
}

/// The only gate surface for a resolved verified app action.
pub(crate) struct VerifiedActionGate;

impl VerifiedActionGate {
    /// Consumes one resolved action and one fresh policy snapshot exactly once.
    pub(crate) fn evaluate(
        action: VerifiedAppAction<Resolved>,
        fresh_policy: FreshPolicySnapshot,
    ) -> VerifiedGateVerdict {
        let VerifiedAppAction {
            transport,
            binding,
            state: Resolved { state },
        } = action;

        let restricted = fresh_policy.has_identity_filter();
        let allowed = match &state {
            ResolvedState::AppOnly | ResolvedState::Unavailable(_) => !restricted,
            ResolvedState::Represented(principal) => !restricted || fresh_policy.admits(principal),
        };

        if !allowed {
            return VerifiedGateVerdict::Deny;
        }

        let provenance = match state {
            ResolvedState::AppOnly => AdmissionProvenance::AppOnly,
            ResolvedState::Represented(principal) => AdmissionProvenance::Represented(principal),
            ResolvedState::Unavailable(failure) => AdmissionProvenance::Unavailable(failure),
        };

        VerifiedGateVerdict::Allow(AdmissionFacts {
            binding,
            provider: transport.provider,
            provenance,
            policy_generation: fresh_policy.generation,
            policy_fingerprint: fresh_policy.fingerprint,
            admitted_at: fresh_policy.evaluated_at,
        })
    }
}

#[derive(Debug)]
enum AdmissionProvenance {
    AppOnly,
    Represented(RepresentedPrincipal),
    Unavailable(ResolutionFailure),
}

/// A non-authoritative, read-only description of admission provenance.
#[derive(Debug)]
#[cfg(test)]
pub(crate) struct AdmissionProvenanceRef<'a> {
    provenance: &'a AdmissionProvenance,
}

#[cfg(test)]
impl AdmissionProvenanceRef<'_> {
    /// Exhaustively folds the sealed provenance without exposing construction.
    pub(crate) fn fold<T>(
        self,
        app_only: impl FnOnce() -> T,
        represented: impl FnOnce(&RepresentedPrincipal) -> T,
        unavailable: impl FnOnce(&ResolutionFailure) -> T,
    ) -> T {
        match self.provenance {
            AdmissionProvenance::AppOnly => app_only(),
            AdmissionProvenance::Represented(principal) => represented(principal),
            AdmissionProvenance::Unavailable(failure) => unavailable(failure),
        }
    }

    /// Returns whether the approved application acted only as itself.
    pub(crate) fn is_app_only(&self) -> bool {
        matches!(self.provenance, AdmissionProvenance::AppOnly)
    }

    /// Returns the represented principal, when one was verified.
    pub(crate) fn represented(&self) -> Option<&RepresentedPrincipal> {
        match self.provenance {
            AdmissionProvenance::Represented(principal) => Some(principal),
            AdmissionProvenance::AppOnly | AdmissionProvenance::Unavailable(_) => None,
        }
    }

    /// Returns the resolution failure when provenance remained unavailable.
    pub(crate) fn unavailable(&self) -> Option<&ResolutionFailure> {
        match self.provenance {
            AdmissionProvenance::Unavailable(failure) => Some(failure),
            AdmissionProvenance::AppOnly | AdmissionProvenance::Represented(_) => None,
        }
    }
}

/// Immutable lifecycle lineage minted only by an allow verdict.
#[derive(Debug)]
pub(crate) struct AdmissionFacts {
    binding: EventBinding,
    provider: TransportProvider,
    provenance: AdmissionProvenance,
    policy_generation: PolicyGeneration,
    policy_fingerprint: PolicyFingerprint,
    admitted_at: DateTime<Utc>,
}

impl AdmissionFacts {
    /// Returns the exact event binding admitted by the gate.
    #[cfg(test)]
    pub(crate) fn binding(&self) -> &EventBinding {
        &self.binding
    }

    /// Returns the verified transport provider.
    #[cfg(test)]
    pub(crate) fn provider(&self) -> TransportProvider {
        self.provider
    }

    /// Returns a non-authoritative view of represented provenance.
    #[cfg(test)]
    pub(crate) fn provenance(&self) -> AdmissionProvenanceRef<'_> {
        AdmissionProvenanceRef {
            provenance: &self.provenance,
        }
    }

    /// Returns the policy generation used by the allowing gate.
    #[cfg(test)]
    pub(crate) fn policy_generation(&self) -> &PolicyGeneration {
        &self.policy_generation
    }

    /// Returns the policy fingerprint used by the allowing gate.
    #[cfg(test)]
    pub(crate) fn policy_fingerprint(&self) -> &PolicyFingerprint {
        &self.policy_fingerprint
    }

    /// Consumes gate admission into durable lifecycle facts, not a reusable proof.
    pub(crate) fn into_lifecycle(self) -> LifecycleAdmissionFacts {
        let context = match self.binding.context {
            EventContext::Guild(guild_id) => LifecycleContext::Guild(guild_id),
            EventContext::DirectMessage => LifecycleContext::DirectMessage,
        };
        let provenance = match self.provenance {
            AdmissionProvenance::AppOnly => LifecycleProvenance::AppOnly,
            AdmissionProvenance::Represented(principal) => LifecycleProvenance::Represented {
                discord_user_id: principal.discord_user_id,
                system_id: principal.system_id,
                member_id: principal.member_id,
            },
            AdmissionProvenance::Unavailable(failure) => {
                LifecycleProvenance::Unavailable(failure.class())
            }
        };
        LifecycleAdmissionFacts {
            message_id: self.binding.message_id,
            channel_id: self.binding.channel_id,
            context,
            webhook_id: self.binding.webhook_id,
            provider: self.provider,
            provenance,
            policy_generation: self.policy_generation.0,
            policy_fingerprint: self.policy_fingerprint.0,
            admitted_at: self.admitted_at,
        }
    }
}

/// Durable non-proof context retained for passive lifecycle transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleContext {
    Guild(GuildId),
    DirectMessage,
}

/// Durable semantic facts retained independently of identity-cache TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleProvenance {
    AppOnly,
    Represented {
        discord_user_id: UserId,
        system_id: Option<Uuid>,
        member_id: Option<Uuid>,
    },
    Unavailable(ResolutionFailureClass),
}

/// Consumed gate output suitable for bounded lifecycle retention.
#[derive(Debug)]
pub(crate) struct LifecycleAdmissionFacts {
    message_id: MessageId,
    channel_id: ChannelId,
    context: LifecycleContext,
    webhook_id: WebhookId,
    provider: TransportProvider,
    provenance: LifecycleProvenance,
    // Retained as durable admission audit metadata even though current lifecycle
    // decisions do not branch on it. `allow` rather than `expect`: fulfillment is
    // toolchain/context dependent (see `DiscordTransportVerifier::private`).
    #[allow(dead_code)]
    policy_generation: u64,
    // Retained as durable admission audit metadata even though current lifecycle
    // decisions do not branch on it. `allow` rather than `expect`: fulfillment is
    // toolchain/context dependent (see `DiscordTransportVerifier::private`).
    #[allow(dead_code)]
    policy_fingerprint: [u8; 32],
    // Retained as durable admission audit metadata even though current lifecycle
    // decisions do not branch on it. `allow` rather than `expect`: fulfillment is
    // toolchain/context dependent (see `DiscordTransportVerifier::private`).
    #[allow(dead_code)]
    admitted_at: DateTime<Utc>,
}

impl LifecycleAdmissionFacts {
    pub(crate) fn message_id(&self) -> MessageId {
        self.message_id
    }

    pub(crate) fn channel_id(&self) -> ChannelId {
        self.channel_id
    }

    pub(crate) fn context(&self) -> LifecycleContext {
        self.context
    }

    pub(crate) fn webhook_id(&self) -> WebhookId {
        self.webhook_id
    }

    pub(crate) fn provider(&self) -> TransportProvider {
        self.provider
    }

    pub(crate) fn provenance(&self) -> &LifecycleProvenance {
        &self.provenance
    }

    pub(crate) fn same_actor_lineage(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.webhook_id == other.webhook_id
            && match (&self.provenance, &other.provenance) {
                (LifecycleProvenance::AppOnly, LifecycleProvenance::AppOnly) => true,
                (
                    LifecycleProvenance::Represented {
                        discord_user_id: a_user,
                        system_id: a_system,
                        member_id: a_member,
                    },
                    LifecycleProvenance::Represented {
                        discord_user_id: b_user,
                        system_id: b_system,
                        member_id: b_member,
                    },
                ) => a_user == b_user && a_system == b_system && a_member == b_member,
                (LifecycleProvenance::Unavailable(_), LifecycleProvenance::Unavailable(_)) => true,
                _ => false,
            }
    }
}

#[cfg(test)]
pub(crate) fn test_lifecycle_facts(
    message_id: MessageId,
    channel_id: ChannelId,
    guild_id: GuildId,
    webhook_id: WebhookId,
    provenance: LifecycleProvenance,
) -> LifecycleAdmissionFacts {
    LifecycleAdmissionFacts {
        message_id,
        channel_id,
        context: LifecycleContext::Guild(guild_id),
        webhook_id,
        provider: TransportProvider::PluralKit,
        provenance,
        policy_generation: 7,
        policy_fingerprint: [7; 32],
        admitted_at: Utc::now(),
    }
}

#[cfg(test)]
pub(super) fn test_admission_facts(
    message_id: MessageId,
    channel_id: ChannelId,
    guild_id: GuildId,
    webhook_id: WebhookId,
) -> AdmissionFacts {
    AdmissionFacts {
        binding: EventBinding {
            message_id,
            channel_id,
            context: EventContext::Guild(guild_id),
            webhook_id,
        },
        provider: TransportProvider::PluralKit,
        provenance: AdmissionProvenance::AppOnly,
        policy_generation: PolicyGeneration(1),
        policy_fingerprint: PolicyFingerprint([1; 32]),
        admitted_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn unresolved() -> VerifiedAppAction<Unresolved> {
        VerifiedAppAction {
            transport: VerifiedTransport {
                provider: TransportProvider::PluralKit,
            },
            binding: EventBinding {
                message_id: MessageId::new(10),
                channel_id: ChannelId::new(20),
                context: EventContext::Guild(GuildId::new(30)),
                webhook_id: WebhookId::new(40),
            },
            state: Unresolved { private: () },
        }
    }

    #[test]
    fn provider_selection_reinterprets_creator_facts() {
        assert_eq!(
            TransportProvider::from_creator_user_id(Some(PLURALKIT_APPLICATION_ID)),
            Some(TransportProvider::PluralKit)
        );
        assert_eq!(TransportProvider::from_creator_user_id(Some(1)), None);
        assert_eq!(TransportProvider::from_creator_user_id(None), None);
    }

    fn policy(restricted: bool, allowed_user: Option<UserId>) -> FreshPolicySnapshot {
        FreshPolicySnapshot {
            restriction_intended: restricted,
            allowed_discord_users: allowed_user.into_iter().collect(),
            allowed_systems: HashSet::new(),
            allowed_members: HashSet::new(),
            generation: PolicyGeneration(7),
            fingerprint: PolicyFingerprint([9; 32]),
            evaluated_at: Utc::now(),
        }
    }

    fn allow_facts(verdict: VerifiedGateVerdict) -> AdmissionFacts {
        match verdict {
            VerifiedGateVerdict::Allow(facts) => facts,
            VerifiedGateVerdict::Deny => panic!("expected the verified action to be allowed"),
        }
    }

    fn resolved_for_test(kind: u8) -> VerifiedAppAction<Resolved> {
        let resolver = PrincipalResolver::new();
        let session = resolver.begin(unresolved());
        match kind {
            0 => resolver.finish_app_only(session),
            1 => resolver.finish_represented(
                session,
                RepresentedPrincipal {
                    discord_user_id: UserId::new(50),
                    system_id: None,
                    member_id: None,
                },
            ),
            _ => resolver.finish_unavailable(
                session,
                ResolutionFailure {
                    kind: ResolutionFailureKind::Network,
                },
            ),
        }
    }

    fn verdict_signature(verdict: VerifiedGateVerdict) -> (bool, u8, Option<u64>) {
        match verdict {
            VerifiedGateVerdict::Deny => (false, 0, None),
            VerifiedGateVerdict::Allow(facts) => facts.provenance().fold(
                || (true, 1, None),
                |principal| (true, 2, Some(principal.discord_user_id().get())),
                |_| (true, 3, None),
            ),
        }
    }

    #[test]
    fn represented_transition_preserves_binding_provider_and_admission_metadata() {
        let resolver = PrincipalResolver::new();
        let session = resolver.begin(unresolved());
        assert_eq!(session.message_id(), MessageId::new(10));
        assert_eq!(session.binding().channel_id(), ChannelId::new(20));
        assert_eq!(session.provider(), TransportProvider::PluralKit);

        let represented_user = UserId::new(50);
        let action = resolver.finish_represented(
            session,
            RepresentedPrincipal {
                discord_user_id: represented_user,
                system_id: None,
                member_id: None,
            },
        );
        let facts = allow_facts(VerifiedActionGate::evaluate(
            action,
            policy(true, Some(represented_user)),
        ));

        assert_eq!(facts.binding().message_id(), MessageId::new(10));
        assert_eq!(facts.binding().channel_id(), ChannelId::new(20));
        assert_eq!(
            facts.binding().context(),
            &EventContext::Guild(GuildId::new(30))
        );
        assert_eq!(facts.binding().webhook_id(), WebhookId::new(40));
        assert_eq!(facts.provider(), TransportProvider::PluralKit);
        assert_eq!(
            facts
                .provenance()
                .represented()
                .map(RepresentedPrincipal::discord_user_id),
            Some(represented_user)
        );
        assert_eq!(facts.policy_generation().get(), 7);
        assert_eq!(facts.policy_fingerprint().as_bytes(), &[9; 32]);
    }

    #[test]
    fn app_only_and_unavailable_allow_only_without_identity_restriction() {
        let resolver = PrincipalResolver::new();
        let app_only = resolver.finish_app_only(resolver.begin(unresolved()));
        assert!(matches!(
            VerifiedActionGate::evaluate(app_only, policy(true, None)),
            VerifiedGateVerdict::Deny
        ));

        let app_only = resolver.finish_app_only(resolver.begin(unresolved()));
        let facts = allow_facts(VerifiedActionGate::evaluate(app_only, policy(false, None)));
        assert!(facts.provenance().is_app_only());

        let unavailable = resolver.finish_unavailable(
            resolver.begin(unresolved()),
            ResolutionFailure {
                kind: ResolutionFailureKind::Deadline,
            },
        );
        assert!(matches!(
            VerifiedActionGate::evaluate(unavailable, policy(true, None)),
            VerifiedGateVerdict::Deny
        ));

        let unavailable = resolver.finish_unavailable(
            resolver.begin(unresolved()),
            ResolutionFailure {
                kind: ResolutionFailureKind::Network,
            },
        );
        let facts = allow_facts(VerifiedActionGate::evaluate(
            unavailable,
            policy(false, None),
        ));
        assert!(facts.provenance().unavailable().is_some());
    }

    #[test]
    fn represented_system_and_member_each_match_and_nonmatch() {
        let system_id = Uuid::new_v4();
        let member_id = Uuid::new_v4();
        for (allowed_systems, allowed_members) in [
            (HashSet::from([system_id]), HashSet::new()),
            (HashSet::new(), HashSet::from([member_id])),
        ] {
            let resolver = PrincipalResolver::new();
            let action = resolver.finish_represented(
                resolver.begin(unresolved()),
                RepresentedPrincipal {
                    discord_user_id: UserId::new(50),
                    system_id: Some(system_id),
                    member_id: Some(member_id),
                },
            );
            let policy = FreshPolicySnapshot {
                restriction_intended: true,
                allowed_discord_users: HashSet::new(),
                allowed_systems,
                allowed_members,
                generation: PolicyGeneration(8),
                fingerprint: PolicyFingerprint([8; 32]),
                evaluated_at: Utc::now(),
            };
            assert!(matches!(
                VerifiedActionGate::evaluate(action, policy),
                VerifiedGateVerdict::Allow(_)
            ));
        }

        let resolver = PrincipalResolver::new();
        let action = resolver.finish_represented(
            resolver.begin(unresolved()),
            RepresentedPrincipal {
                discord_user_id: UserId::new(50),
                system_id: Some(system_id),
                member_id: Some(member_id),
            },
        );
        let policy = FreshPolicySnapshot {
            restriction_intended: true,
            allowed_discord_users: HashSet::new(),
            allowed_systems: HashSet::from([Uuid::new_v4()]),
            allowed_members: HashSet::from([Uuid::new_v4()]),
            generation: PolicyGeneration(9),
            fingerprint: PolicyFingerprint([9; 32]),
            evaluated_at: Utc::now(),
        };
        assert!(matches!(
            VerifiedActionGate::evaluate(action, policy),
            VerifiedGateVerdict::Deny
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn gate_is_deterministic_for_identical_linear_inputs(
            kind in 0u8..3,
            restricted in any::<bool>(),
            represented_matches in any::<bool>(),
        ) {
            let allowed = (kind == 1 && represented_matches).then_some(UserId::new(50));
            let first = verdict_signature(VerifiedActionGate::evaluate(
                resolved_for_test(kind),
                policy(restricted, allowed),
            ));
            let second = verdict_signature(VerifiedActionGate::evaluate(
                resolved_for_test(kind),
                policy(restricted, allowed),
            ));
            let expected_allow = !restricted || (kind == 1 && represented_matches);

            prop_assert_eq!(first, second);
            prop_assert_eq!(first.0, expected_allow);
            if expected_allow {
                prop_assert_eq!(first.1, kind + 1);
            }
        }
    }
}
