//! Bounded gateway observations, independent of ordinary notification authority.

use super::{
    events::Handler,
    messaging::queries::{DiscordNativeObservation, MessageView},
    verified_action::{
        AdmissionFacts, DiscordTransportVerifier, Resolved, VerifiedActionGate, VerifiedAppAction,
        VerifiedGateVerdict,
    },
    verified_action_runtime::{
        VerifiedUpdateCandidate, fresh_policy_snapshot, resolve_webhook_action,
    },
};
use crate::{
    config::LoadedConfig,
    core::{
        ModuleInstanceId,
        context::ObservationOrigin,
        observation::{MessageObservation, MessageOccurrence},
    },
};
use chrono::{DateTime, Utc};
use serenity::{
    http::Http,
    model::{event::MessageUpdateEvent, prelude::*},
};
use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};

const OBSERVATION_DEADLINE: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_OBSERVATIONS: usize = 4;
const MAX_OBSERVATION_BYTES: usize = 1_048_576;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationFailure {
    Busy,
    Timeout,
    ReadForbidden,
    ReadFailed,
    InvalidSource,
    RevisionMismatch,
    ConflictingFields,
    MissingIdentity,
    UnverifiedTransport,
    PolicyUnavailable,
    InvalidTimestamp,
    TooLarge,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Exclusion {
    Scope,
    Sender,
    Ignored,
    Muted,
    SelfMessage,
    NonEdit,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchOutcome {
    Disabled,
    Filtered(Exclusion),
    Offered,
    Incomplete(ObservationFailure),
    SinkUnavailable,
    Full,
}
impl DispatchOutcome {
    pub(crate) fn record(self, channel: ChannelId, message: MessageId) {
        if matches!(
            self,
            Self::Incomplete(_) | Self::SinkUnavailable | Self::Full
        ) {
            tracing::warn!(channel_id = channel.get(), message_id = message.get(), outcome = ?self, "gateway observation not offered");
        }
    }
}

/// Receiver payload has no lifecycle admission proof or recursive parent body.
#[derive(Debug)]
pub(crate) struct DiscordMessageObservation {
    pub(crate) common: MessageObservation,
    pub(crate) native: DiscordNativeObservation,
    pub(crate) thread_parent: Option<ChannelId>,
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "recipient modes are constructed by the subsequent durable owner"
    )
)]
#[derive(Clone, Debug)]
pub(crate) enum SenderFilter {
    EveryoneInRoom,
    Only(HashSet<UserId>),
}
#[derive(Clone, Debug)]
pub(crate) struct RoomScope {
    pub(crate) guild: GuildId,
    pub(crate) channel: ChannelId,
    pub(crate) include_threads: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct ObservationPolicy {
    pub(crate) rooms: Vec<RoomScope>,
    pub(crate) denied_rooms: HashSet<(GuildId, ChannelId)>,
    pub(crate) senders: SenderFilter,
    pub(crate) own_actors: HashSet<UserId>,
    pub(crate) own_webhooks: HashSet<WebhookId>,
}
impl ObservationPolicy {
    fn valid(&self) -> bool {
        !self.rooms.is_empty()
            && !self.own_actors.is_empty()
            && !matches!(&self.senders, SenderFilter::Only(actors) if actors.is_empty())
    }
    fn room(&self, guild: Option<GuildId>, channel: ChannelId, parent: Option<u64>) -> bool {
        if guild.is_some_and(|guild| self.denied_rooms.contains(&(guild, channel))) {
            return false;
        }
        self.rooms.iter().any(|scope| {
            Some(scope.guild) == guild
                && (scope.channel == channel
                    || scope.include_threads && Some(scope.channel.get()) == parent)
        })
    }
    fn sender(&self, actor: UserId) -> bool {
        match &self.senders {
            SenderFilter::EveryoneInRoom => true,
            SenderFilter::Only(actors) => actors.contains(&actor),
        }
    }
}

struct EnabledIngress {
    sender: mpsc::Sender<DiscordMessageObservation>,
    policy: Arc<RwLock<ObservationPolicy>>,
    preprocessing: Semaphore,
}

pub(crate) struct ObservationIngress {
    source: ModuleInstanceId,
    enabled: Option<EnabledIngress>,
}

/// An enabled observation may share one consumed gate verdict with legacy delivery.
/// A rejected observation scope alone never rejects the legacy path.
pub(super) enum PreparedAdmission {
    Unobserved,
    Rejected,
    Admitted {
        facts: AdmissionFacts,
        generation: u64,
    },
}
impl PreparedAdmission {
    pub(super) fn take(self, generation: u64) -> Result<Option<AdmissionFacts>, ()> {
        match self {
            Self::Unobserved => Ok(None),
            Self::Rejected => Err(()),
            Self::Admitted {
                facts,
                generation: admitted,
            } if admitted == generation => Ok(Some(facts)),
            Self::Admitted { .. } => Err(()),
        }
    }
}
pub(super) struct PreparedObservation {
    pub(super) admission: PreparedAdmission,
    pub(super) parent: Option<Option<u64>>,
    pub(super) outcome: DispatchOutcome,
}
impl PreparedObservation {
    fn new(outcome: DispatchOutcome) -> Self {
        Self {
            admission: PreparedAdmission::Unobserved,
            parent: None,
            outcome,
        }
    }
}

impl ObservationIngress {
    pub(crate) fn disabled(source: ModuleInstanceId) -> Self {
        Self {
            source,
            enabled: None,
        }
    }

    // The durable owner will attach this same bounded interface; production startup is disabled.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "enabled construction belongs to the next durable-owner atom"
        )
    )]
    pub(crate) fn enabled(
        source: ModuleInstanceId,
        policy: Arc<RwLock<ObservationPolicy>>,
        capacity: std::num::NonZeroUsize,
    ) -> Result<(Self, mpsc::Receiver<DiscordMessageObservation>), ObservationFailure> {
        if !policy
            .read()
            .map_err(|_| ObservationFailure::PolicyUnavailable)?
            .valid()
        {
            return Err(ObservationFailure::MissingIdentity);
        }
        let (sender, receiver) = mpsc::channel(capacity.get());
        Ok((
            Self {
                source,
                enabled: Some(EnabledIngress {
                    sender,
                    policy,
                    preprocessing: Semaphore::new(MAX_CONCURRENT_OBSERVATIONS),
                }),
            },
            receiver,
        ))
    }

    fn available(&self) -> Result<&EnabledIngress, DispatchOutcome> {
        let enabled = self.enabled.as_ref().ok_or(DispatchOutcome::Disabled)?;
        if enabled.sender.is_closed() {
            return Err(DispatchOutcome::SinkUnavailable);
        }
        if enabled.sender.capacity() == 0 {
            return Err(DispatchOutcome::Full);
        }
        Ok(enabled)
    }

    async fn bounded<F>(&self, work: F) -> PreparedObservation
    where
        F: std::future::Future<Output = PreparedObservation>,
    {
        let enabled = match self.available() {
            Ok(enabled) => enabled,
            Err(outcome) => return PreparedObservation::new(outcome),
        };
        let Ok(_permit) = enabled.preprocessing.try_acquire() else {
            return PreparedObservation::new(DispatchOutcome::Incomplete(ObservationFailure::Busy));
        };
        tokio::time::timeout(OBSERVATION_DEADLINE, work)
            .await
            .unwrap_or_else(|_| {
                PreparedObservation::new(DispatchOutcome::Incomplete(ObservationFailure::Timeout))
            })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "cheap checks distinguish unknown topology and transport from known direct actors"
    )]
    fn preflight(
        &self,
        config: &LoadedConfig,
        guild: Option<GuildId>,
        channel: ChannelId,
        parent: Option<Option<u64>>,
        author: Option<&User>,
        webhook: Option<Option<WebhookId>>,
        bot: u64,
    ) -> Result<(), DispatchOutcome> {
        let enabled = self.available()?;
        let policy = enabled
            .policy
            .read()
            .map_err(|_| DispatchOutcome::Incomplete(ObservationFailure::PolicyUnavailable))?;
        if !policy.valid() || bot == 0 {
            return Err(DispatchOutcome::Incomplete(
                ObservationFailure::MissingIdentity,
            ));
        }
        if guild.is_some_and(|guild| policy.denied_rooms.contains(&(guild, channel))) {
            return Err(DispatchOutcome::Filtered(Exclusion::Scope));
        }
        let in_scope = match parent {
            Some(parent) => policy.room(guild, channel, parent),
            None => policy.rooms.iter().any(|scope| {
                Some(scope.guild) == guild && (scope.channel == channel || scope.include_threads)
            }),
        };
        if !in_scope {
            return Err(DispatchOutcome::Filtered(Exclusion::Scope));
        }
        if guild.is_some_and(|guild| {
            crate::mute_store::global().is_some_and(|store| store.is_guild_muted(guild.get()))
        }) {
            return Err(DispatchOutcome::Filtered(Exclusion::Muted));
        }
        if webhook
            .flatten()
            .is_some_and(|id| policy.own_webhooks.contains(&id))
        {
            return Err(DispatchOutcome::Filtered(Exclusion::SelfMessage));
        }
        let gate = parent.map_or(channel.get(), |parent| {
            super::events::select_gate_channel(config, channel.get(), parent)
        });
        let channel_policy = config.channel_policy(gate);
        if parent.is_some() && channel_policy.is_none() {
            return Err(DispatchOutcome::Filtered(Exclusion::Scope));
        }
        if let Some(author) = author {
            if author.id.get() == bot || policy.own_actors.contains(&author.id) {
                return Err(DispatchOutcome::Filtered(Exclusion::SelfMessage));
            }
            if webhook == Some(None) {
                if config.is_ignored(author.id.get()) {
                    return Err(DispatchOutcome::Filtered(Exclusion::Ignored));
                }
                if !policy.sender(author.id)
                    || author.bot && !config.is_allowed(author.id.get())
                    || channel_policy.is_some_and(|policy| {
                        !crate::gate::direct_sender_allowed(policy, author.id.get())
                    })
                {
                    return Err(DispatchOutcome::Filtered(Exclusion::Sender));
                }
            }
        }
        Ok(())
    }

    pub(super) async fn create(
        &self,
        handler: &Handler,
        http: &Http,
        message: &Message,
    ) -> PreparedObservation {
        self.create_with(
            message,
            handler
                .bot_user_id
                .load(std::sync::atomic::Ordering::Relaxed),
            || crate::config::load_config(&handler.state_dir),
            super::events::resolve_thread_parent(http, &handler.state, message.channel_id.get()),
            |message: Message| async move {
                let config = crate::config::load_config(&handler.state_dir);
                let action = DiscordTransportVerifier::new()
                    .verify(
                        http,
                        &handler.state,
                        &config.trusted_webhook_creators,
                        message,
                    )
                    .await?;
                resolve_webhook_action(handler.pk_resolver.as_deref(), action).await
            },
        )
        .await
    }

    pub(super) async fn create_with<L, T, V, VF>(
        &self,
        message: &Message,
        bot: u64,
        load: L,
        topology: T,
        verification: V,
    ) -> PreparedObservation
    where
        L: Fn() -> Arc<LoadedConfig>,
        T: std::future::Future<Output = Option<u64>>,
        V: FnOnce(Message) -> VF,
        VF: std::future::Future<Output = Option<VerifiedAppAction<Resolved>>>,
    {
        self.bounded(async {
            if let Err(outcome) = self.preflight(
                &load(),
                message.guild_id,
                message.channel_id,
                None,
                Some(&message.author),
                Some(message.webhook_id),
                bot,
            ) {
                return PreparedObservation::new(outcome);
            }
            let parent = topology.await;
            self.complete_with(
                message,
                parent,
                MessageOccurrence::Created,
                bot,
                &load,
                verification,
            )
            .await
        })
        .await
    }

    pub(super) async fn edit(
        &self,
        handler: &Handler,
        http: &Http,
        event: &MessageUpdateEvent,
        old: Option<&Message>,
        new: Option<&Message>,
    ) -> PreparedObservation {
        self.edit_with(
            event,
            old,
            new,
            handler
                .bot_user_id
                .load(std::sync::atomic::Ordering::Relaxed),
            || crate::config::load_config(&handler.state_dir),
            super::events::resolve_thread_parent(http, &handler.state, event.channel_id.get()),
            http.get_message(event.channel_id, event.id),
            |message: Message| async move {
                let config = crate::config::load_config(&handler.state_dir);
                let action = DiscordTransportVerifier::new()
                    .verify(
                        http,
                        &handler.state,
                        &config.trusted_webhook_creators,
                        message,
                    )
                    .await?;
                resolve_webhook_action(handler.pk_resolver.as_deref(), action).await
            },
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "actual gateway edit orchestration injects only its existing IO futures"
    )]
    pub(super) async fn edit_with<L, T, R, V, VF>(
        &self,
        event: &MessageUpdateEvent,
        old: Option<&Message>,
        new: Option<&Message>,
        bot: u64,
        load: L,
        topology: T,
        read: R,
        verification: V,
    ) -> PreparedObservation
    where
        L: Fn() -> Arc<LoadedConfig>,
        T: std::future::Future<Output = Option<u64>>,
        R: std::future::Future<Output = Result<Message, serenity::Error>>,
        V: FnOnce(Message) -> VF,
        VF: std::future::Future<Output = Option<VerifiedAppAction<Resolved>>>,
    {
        self.bounded(async {
            let Some(revision) = event.edited_timestamp else {
                return PreparedObservation::new(DispatchOutcome::Filtered(Exclusion::NonEdit));
            };
            if VerifiedUpdateCandidate::from_gateway(event, old, new).is_err() {
                return PreparedObservation::new(DispatchOutcome::Incomplete(
                    ObservationFailure::ConflictingFields,
                ));
            }
            let author = event
                .author
                .as_ref()
                .or_else(|| old.map(|message| &message.author));
            let webhook = event
                .webhook_id
                .or_else(|| old.map(|message| message.webhook_id));
            if let Err(outcome) = self.preflight(
                &load(),
                event.guild_id,
                event.channel_id,
                None,
                author,
                webhook,
                bot,
            ) {
                return PreparedObservation::new(outcome);
            }
            let parent = topology.await;
            if let Err(outcome) = self.preflight(
                &load(),
                event.guild_id,
                event.channel_id,
                Some(parent),
                author,
                webhook,
                bot,
            ) {
                return PreparedObservation::new(outcome);
            }
            // Serenity loses explicit-null reference presence; a cache update can retain the old reference at the new revision.
            let current = if event.message_reference.is_some() {
                match new {
                    Some(message) => message.clone(),
                    None => match read.await {
                        Ok(message) => message,
                        Err(error) => {
                            return PreparedObservation::new(DispatchOutcome::Incomplete(
                                read_failure(error),
                            ));
                        }
                    },
                }
            } else {
                match read.await {
                    Ok(message) => message,
                    Err(error) => {
                        return PreparedObservation::new(DispatchOutcome::Incomplete(
                            read_failure(error),
                        ));
                    }
                }
            };
            if let Err(failure) = validate_edit(event, old, &current) {
                return PreparedObservation::new(DispatchOutcome::Incomplete(failure));
            }
            let revision = match instant(revision) {
                Ok(revision) => revision,
                Err(failure) => {
                    return PreparedObservation::new(DispatchOutcome::Incomplete(failure));
                }
            };
            self.complete_with(
                &current,
                parent,
                MessageOccurrence::Edited { revision },
                bot,
                &load,
                verification,
            )
            .await
        })
        .await
    }

    async fn complete_with<L, V, VF>(
        &self,
        message: &Message,
        parent: Option<u64>,
        occurrence: MessageOccurrence,
        bot: u64,
        load: &L,
        verification: V,
    ) -> PreparedObservation
    where
        L: Fn() -> Arc<LoadedConfig>,
        V: FnOnce(Message) -> VF,
        VF: std::future::Future<Output = Option<VerifiedAppAction<Resolved>>>,
    {
        let config = load();
        if let Err(outcome) = self.preflight(
            &config,
            message.guild_id,
            message.channel_id,
            Some(parent),
            Some(&message.author),
            Some(message.webhook_id),
            bot,
        ) {
            return PreparedObservation::new(outcome);
        }
        let action = if message.webhook_id.is_some() {
            verification(message.clone()).await
        } else {
            None
        };
        let fresh = load();
        if fresh.generation() != config.generation() {
            return PreparedObservation {
                admission: PreparedAdmission::Rejected,
                parent: Some(parent),
                outcome: DispatchOutcome::Incomplete(ObservationFailure::PolicyUnavailable),
            };
        }
        let mut prepared = PreparedObservation {
            admission: PreparedAdmission::Unobserved,
            parent: Some(parent),
            outcome: DispatchOutcome::Disabled,
        };
        let facts = if message.webhook_id.is_some() {
            prepared.admission = PreparedAdmission::Rejected;
            let Some(action) = action else {
                prepared.outcome =
                    DispatchOutcome::Incomplete(ObservationFailure::UnverifiedTransport);
                return prepared;
            };
            let binding = action.binding();
            let context_matches = match (binding.context(), message.guild_id) {
                (super::verified_action::EventContext::Guild(bound), Some(guild)) => {
                    *bound == guild
                }
                (super::verified_action::EventContext::DirectMessage, None) => true,
                _ => false,
            };
            if binding.message_id() != message.id
                || binding.channel_id() != message.channel_id
                || Some(binding.webhook_id()) != message.webhook_id
                || !context_matches
            {
                prepared.outcome =
                    DispatchOutcome::Incomplete(ObservationFailure::ConflictingFields);
                return prepared;
            }
            let gate = super::events::select_gate_channel(&fresh, message.channel_id.get(), parent);
            let Some(policy) = fresh_policy_snapshot(&fresh, gate) else {
                prepared.outcome = DispatchOutcome::Filtered(Exclusion::Scope);
                return prepared;
            };
            match VerifiedActionGate::evaluate(action, policy) {
                VerifiedGateVerdict::Allow(facts) => Some(facts),
                VerifiedGateVerdict::Deny => {
                    prepared.outcome = DispatchOutcome::Filtered(Exclusion::Sender);
                    return prepared;
                }
            }
        } else {
            None
        };
        let author = facts
            .as_ref()
            .map_or(ObservationAuthor::Direct, ObservationAuthor::Verified);
        prepared.outcome = self.offer(&fresh, message, parent, occurrence, author, bot);
        if let Some(facts) = facts {
            prepared.admission = PreparedAdmission::Admitted {
                facts,
                generation: fresh.generation(),
            };
        }
        prepared
    }

    fn offer(
        &self,
        config: &LoadedConfig,
        message: &Message,
        parent: Option<u64>,
        occurrence: MessageOccurrence,
        author: ObservationAuthor<'_>,
        bot: u64,
    ) -> DispatchOutcome {
        let (principal, verified) = match author {
            ObservationAuthor::Direct => (message.author.id, false),
            ObservationAuthor::Verified(facts) => {
                let provenance = facts.provenance();
                if provenance.unavailable().is_some() {
                    return DispatchOutcome::Incomplete(ObservationFailure::UnverifiedTransport);
                }
                (
                    provenance
                        .represented()
                        .map_or(message.author.id, |principal| principal.discord_user_id()),
                    true,
                )
            }
        };
        let enabled = match self.available() {
            Ok(value) => value,
            Err(outcome) => return outcome,
        };
        let policy = match enabled.policy.read() {
            Ok(policy) => policy,
            Err(_) => return DispatchOutcome::Incomplete(ObservationFailure::PolicyUnavailable),
        };
        if !policy.valid() || bot == 0 {
            return DispatchOutcome::Incomplete(ObservationFailure::MissingIdentity);
        }
        if !policy.room(message.guild_id, message.channel_id, parent) {
            return DispatchOutcome::Filtered(Exclusion::Scope);
        }
        if message.author.id.get() == bot
            || principal.get() == bot
            || policy.own_actors.contains(&message.author.id)
            || policy.own_actors.contains(&principal)
            || message
                .webhook_id
                .is_some_and(|id| policy.own_webhooks.contains(&id))
        {
            return DispatchOutcome::Filtered(Exclusion::SelfMessage);
        }
        if config.is_ignored(principal.get()) {
            return DispatchOutcome::Filtered(Exclusion::Ignored);
        }
        if message.guild_id.is_some_and(|id| {
            crate::mute_store::global().is_some_and(|store| store.is_guild_muted(id.get()))
        }) {
            return DispatchOutcome::Filtered(Exclusion::Muted);
        }
        let gate = super::events::select_gate_channel(config, message.channel_id.get(), parent);
        let Some(channel) = config.channel_policy(gate) else {
            return DispatchOutcome::Filtered(Exclusion::Scope);
        };
        if !policy.sender(principal)
            || (!verified
                && (!crate::gate::direct_sender_allowed(channel, principal.get())
                    || message.author.bot && !config.is_allowed(message.author.id.get())))
        {
            return DispatchOutcome::Filtered(Exclusion::Sender);
        }
        if message.webhook_id.is_some() && !verified {
            return DispatchOutcome::Incomplete(ObservationFailure::UnverifiedTransport);
        }
        let observation = match project(&self.source, config, message, parent, occurrence) {
            Ok(value) => value,
            Err(failure) => return DispatchOutcome::Incomplete(failure),
        };
        match enabled.sender.try_send(observation) {
            Ok(()) => DispatchOutcome::Offered,
            Err(mpsc::error::TrySendError::Full(_)) => DispatchOutcome::Full,
            Err(mpsc::error::TrySendError::Closed(_)) => DispatchOutcome::SinkUnavailable,
        }
    }
}

enum ObservationAuthor<'a> {
    Direct,
    Verified(&'a AdmissionFacts),
}

fn read_failure(error: serenity::Error) -> ObservationFailure {
    match error {
        serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(response))
            if response.status_code.as_u16() == 403 =>
        {
            ObservationFailure::ReadForbidden
        }
        serenity::Error::Http(serenity::http::HttpError::Request(error)) if error.is_timeout() => {
            ObservationFailure::Timeout
        }
        _ => ObservationFailure::ReadFailed,
    }
}

fn instant(timestamp: serenity::model::Timestamp) -> Result<DateTime<Utc>, ObservationFailure> {
    let value = timestamp
        .to_rfc3339()
        .ok_or(ObservationFailure::InvalidTimestamp)?;
    DateTime::parse_from_rfc3339(&value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| ObservationFailure::InvalidTimestamp)
}
fn validate_edit(
    event: &MessageUpdateEvent,
    old: Option<&Message>,
    current: &Message,
) -> Result<(), ObservationFailure> {
    VerifiedUpdateCandidate::from_gateway(event, old, Some(current))
        .map_err(|_| ObservationFailure::ConflictingFields)?;
    if event.edited_timestamp.is_none() || current.edited_timestamp != event.edited_timestamp {
        return Err(ObservationFailure::RevisionMismatch);
    }
    if event
        .timestamp
        .is_some_and(|timestamp| timestamp != current.timestamp)
        || event.kind.is_some_and(|kind| kind != current.kind)
        || event
            .application_id
            .is_some_and(|application| application != current.application_id)
        || event
            .author
            .as_ref()
            .is_some_and(|author| author.bot != current.author.bot)
        || event
            .attachments
            .as_ref()
            .is_some_and(|attachments| !same_attachments(attachments, &current.attachments))
        || event.message_reference.as_ref().is_some_and(|reference| {
            !same_reference(reference.as_ref(), current.message_reference.as_ref())
        })
    {
        return Err(ObservationFailure::ConflictingFields);
    }
    Ok(())
}
fn project(
    source: &ModuleInstanceId,
    config: &LoadedConfig,
    message: &Message,
    parent: Option<u64>,
    occurrence: MessageOccurrence,
) -> Result<DiscordMessageObservation, ObservationFailure> {
    // Only retained fields are counted; nested messages never enter the observation.
    let attachments =
        serde_json::to_vec(&message.attachments).map_err(|_| ObservationFailure::InvalidSource)?;
    let summary_bytes = message.attachments.iter().fold(0usize, |sum, attachment| {
        sum.saturating_add(attachment.filename.len())
            .saturating_add(attachment.content_type.as_ref().map_or(0, String::len))
            .saturating_add(std::mem::size_of::<crate::core::context::AttachmentSummary>())
            .saturating_add(std::mem::size_of::<Attachment>())
    });
    if message
        .content
        .len()
        .saturating_add(attachments.len())
        .saturating_add(summary_bytes)
        .saturating_add(message.author.name.len().saturating_mul(2))
        .saturating_add(std::mem::size_of::<DiscordMessageObservation>())
        .saturating_add(
            source
                .provider()
                .as_str()
                .len()
                .saturating_add(source.slot().as_str().len())
                .saturating_mul(8),
        )
        > MAX_OBSERVATION_BYTES
    {
        return Err(ObservationFailure::TooLarge);
    }
    let origin = match occurrence {
        MessageOccurrence::Created => ObservationOrigin::GatewayCreate,
        MessageOccurrence::Edited { .. } => ObservationOrigin::GatewayEdit,
    };
    let view = MessageView::from_discord(config, message, origin)
        .map_err(|_| ObservationFailure::InvalidTimestamp)?;
    let context = view
        .context(source)
        .map_err(|_| ObservationFailure::InvalidTimestamp)?;
    let common =
        MessageObservation::new(context, occurrence).ok_or(ObservationFailure::RevisionMismatch)?;
    Ok(DiscordMessageObservation {
        common,
        native: view.into_native(),
        thread_parent: parent.map(ChannelId::new),
    })
}

fn same_reference(left: Option<&MessageReference>, right: Option<&MessageReference>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.kind == b.kind
                && a.message_id == b.message_id
                && a.channel_id == b.channel_id
                && a.guild_id == b.guild_id
                && a.fail_if_not_exists == b.fail_if_not_exists
        }
        _ => false,
    }
}
fn same_attachments(left: &[Attachment], right: &[Attachment]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.id == b.id
                && a.filename == b.filename
                && a.description == b.description
                && a.height == b.height
                && a.proxy_url == b.proxy_url
                && a.size == b.size
                && a.url == b.url
                && a.width == b.width
                && a.content_type == b.content_type
                && a.ephemeral == b.ephemeral
                && a.duration_secs.map(f64::to_bits) == b.duration_secs.map(f64::to_bits)
                && a.waveform == b.waveform
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config},
        core::{ModuleSlotId, ProviderId},
        gate::{GateDecision, InboundGate},
    };
    use std::num::NonZeroUsize;

    fn source() -> ModuleInstanceId {
        ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("test-room-owner").unwrap(),
        )
    }
    fn config() -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "10".into(),
            require_mention: true,
            allow_from: vec!["20".into()],
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }
    fn message() -> Message {
        serde_json::from_value(serde_json::json!({
            "id":"30", "channel_id":"10", "guild_id":"40", "type":0,
            "author":{"id":"20","username":"sender","discriminator":"0"},
            "content":"ordinary unmentioned conversation", "timestamp":"2026-10-13T12:00:00.123456789Z",
            "edited_timestamp":null, "tts":false, "mention_everyone":false, "mentions":[],
            "mention_roles":[], "attachments":[], "embeds":[], "pinned":false, "flags":0, "components":[]
        })).unwrap()
    }
    fn update() -> MessageUpdateEvent {
        serde_json::from_value(serde_json::json!({"id":"30","channel_id":"10","guild_id":"40","edited_timestamp":"2026-10-13T12:00:01.123456789Z"})).unwrap()
    }
    fn enabled() -> (
        ObservationIngress,
        mpsc::Receiver<DiscordMessageObservation>,
        Arc<RwLock<ObservationPolicy>>,
    ) {
        let policy = Arc::new(RwLock::new(ObservationPolicy {
            rooms: vec![RoomScope {
                guild: GuildId::new(40),
                channel: ChannelId::new(10),
                include_threads: false,
            }],
            denied_rooms: HashSet::new(),
            senders: SenderFilter::EveryoneInRoom,
            own_actors: HashSet::from([UserId::new(90)]),
            own_webhooks: HashSet::new(),
        }));
        let (ingress, receiver) =
            ObservationIngress::enabled(source(), policy.clone(), NonZeroUsize::new(4).unwrap())
                .unwrap();
        (ingress, receiver, policy)
    }
    fn offer(
        ingress: &ObservationIngress,
        message: &Message,
        occurrence: MessageOccurrence,
    ) -> DispatchOutcome {
        ingress.offer(
            &config(),
            message,
            None,
            occurrence,
            ObservationAuthor::Direct,
            99,
        )
    }

    #[test]
    fn actual_offer_accepts_ambient_without_changing_ordinary_attention() {
        let (ingress, mut receiver, _) = enabled();
        let message = message();
        assert_eq!(
            InboundGate::check_guild(&config(), 10, 20, false, Some(40)),
            GateDecision::Drop
        );
        assert_eq!(
            offer(&ingress, &message, MessageOccurrence::Created),
            DispatchOutcome::Offered
        );
        let observed = receiver.try_recv().unwrap();
        assert_eq!(observed.common.context().content(), message.content);
        assert_eq!(
            observed.common.identity().occurrence(),
            &MessageOccurrence::Created
        );
        assert_eq!(
            observed.common.identity().message().conversation().module(),
            &source()
        );
        assert_eq!(observed.native.message_id(), message.id);
        assert!(observed.thread_parent.is_none());
        assert_eq!(
            InboundGate::check_guild(&config(), 10, 20, true, Some(40)),
            GateDecision::Deliver
        );
    }

    #[test]
    fn scope_sender_and_self_exclusions_do_not_offer() {
        let (ingress, mut receiver, policy) = enabled();
        let mut candidate = message();
        candidate.guild_id = Some(GuildId::new(41));
        assert_eq!(
            offer(&ingress, &candidate, MessageOccurrence::Created),
            DispatchOutcome::Filtered(Exclusion::Scope)
        );
        candidate = message();
        candidate.author.id = UserId::new(21);
        assert_eq!(
            offer(&ingress, &candidate, MessageOccurrence::Created),
            DispatchOutcome::Filtered(Exclusion::Sender)
        );
        candidate = message();
        policy
            .write()
            .unwrap()
            .own_actors
            .insert(candidate.author.id);
        assert_eq!(
            offer(&ingress, &candidate, MessageOccurrence::Created),
            DispatchOutcome::Filtered(Exclusion::SelfMessage)
        );
        policy
            .write()
            .unwrap()
            .own_actors
            .remove(&candidate.author.id);
        policy.write().unwrap().senders = SenderFilter::Only(HashSet::from([UserId::new(22)]));
        assert_eq!(
            offer(&ingress, &candidate, MessageOccurrence::Created),
            DispatchOutcome::Filtered(Exclusion::Sender)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn checked_occurrences_do_not_depend_on_arrival_order_or_content_changes() {
        let config = config();
        let mut message = message();
        let created = project(
            &source(),
            &config,
            &message,
            None,
            MessageOccurrence::Created,
        )
        .unwrap();
        let repeated = project(
            &source(),
            &config,
            &message,
            None,
            MessageOccurrence::Created,
        )
        .unwrap();
        assert_eq!(created.common.identity(), repeated.common.identity());
        let first = instant(update().edited_timestamp.unwrap()).unwrap();
        message.edited_timestamp =
            Some(serenity::model::Timestamp::parse("2026-10-13T12:00:01.123456789Z").unwrap());
        let edit = project(
            &source(),
            &config,
            &message,
            None,
            MessageOccurrence::Edited { revision: first },
        )
        .unwrap();
        let second = first + chrono::Duration::seconds(1);
        message.edited_timestamp =
            Some(serenity::model::Timestamp::parse("2026-10-13T12:00:02.123456789Z").unwrap());
        let later = project(
            &source(),
            &config,
            &message,
            None,
            MessageOccurrence::Edited { revision: second },
        )
        .unwrap();
        assert_ne!(edit.common.identity(), later.common.identity());
        assert_ne!(edit.common.identity(), created.common.identity());
        assert_eq!(
            edit.common.context().content(),
            later.common.context().content()
        );
        assert!(
            MessageObservation::new(
                later.common.context().clone(),
                MessageOccurrence::Edited { revision: first }
            )
            .is_none()
        );
        assert_eq!(first.timestamp_subsec_nanos(), 123_456_789);
    }

    #[test]
    fn exact_edit_validation_preserves_attachment_and_reference_semantics() {
        let mut event = update();
        let mut current = message();
        current.edited_timestamp = event.edited_timestamp;
        assert!(validate_edit(&event, None, &current).is_ok());
        event.attachments = Some(vec![]);
        let attachment: Attachment = serde_json::from_value(serde_json::json!({"id":"50","filename":"image.png","size":8,"url":"https://example.invalid/file","proxy_url":"https://example.invalid/proxy"})).unwrap();
        current.attachments.push(attachment);
        assert_eq!(
            validate_edit(&event, None, &current),
            Err(ObservationFailure::ConflictingFields)
        );
        event.attachments = Some(current.attachments.clone());
        assert!(validate_edit(&event, None, &current).is_ok());
        current.message_reference = Some(MessageReference::new(
            MessageReferenceKind::Default,
            ChannelId::new(77),
        ));
        event.message_reference = Some(None);
        assert_eq!(
            validate_edit(&event, None, &current),
            Err(ObservationFailure::ConflictingFields)
        );
        event.message_reference = None;
        assert!(validate_edit(&event, None, &current).is_ok());
        current.edited_timestamp =
            Some(serenity::model::Timestamp::parse("2026-10-13T12:00:02Z").unwrap());
        assert_eq!(
            validate_edit(&event, None, &current),
            Err(ObservationFailure::RevisionMismatch)
        );
    }

    #[test]
    fn bounded_receiver_and_disabled_have_explicit_outcomes() {
        let (ingress, receiver, _) = enabled();
        for _ in 0..4 {
            assert_eq!(
                offer(&ingress, &message(), MessageOccurrence::Created),
                DispatchOutcome::Offered
            );
        }
        assert_eq!(
            offer(&ingress, &message(), MessageOccurrence::Created),
            DispatchOutcome::Full
        );
        assert_eq!(
            InboundGate::check_guild(&config(), 10, 20, true, Some(40)),
            GateDecision::Deliver
        );
        drop(receiver);
        assert_eq!(
            offer(&ingress, &message(), MessageOccurrence::Created),
            DispatchOutcome::SinkUnavailable
        );
        assert_eq!(
            offer(
                &ObservationIngress::disabled(source()),
                &message(),
                MessageOccurrence::Created
            ),
            DispatchOutcome::Disabled
        );
    }

    #[tokio::test]
    async fn disabled_callback_helpers_skip_topology_and_hydration() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let handler = Handler {
            state: crate::state::new_state(),
            queue: Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
                &state_dir,
            ))),
            tx,
            state_dir,
            bot_user_id: std::sync::atomic::AtomicU64::new(0),
            discord_cmd_rx: tokio::sync::Mutex::new(None),
            presence: super::super::presence::SharedPresence::new(),
            pronoun_service: None,
            nameplate_service: None,
            ingress_ledger: Arc::new(crate::ingress_ledger::IngressLedger::new()),
            pk_resolver: None,
            observation: ObservationIngress::disabled(source()),
        };
        let http = Http::new("unused-local-fixture");
        assert_eq!(
            handler
                .observation
                .create(&handler, &http, &message())
                .await
                .outcome,
            DispatchOutcome::Disabled
        );
        assert_eq!(
            handler
                .observation
                .edit(&handler, &http, &update(), None, None)
                .await
                .outcome,
            DispatchOutcome::Disabled
        );
        assert!(handler.state.read().await.thread_parents.is_empty());
    }

    #[test]
    fn parent_coordinates_do_not_authorize_reads() {
        let (ingress, mut receiver, _) = enabled();
        let mut candidate = message();
        candidate.message_reference = Some(MessageReference::new(
            MessageReferenceKind::Default,
            ChannelId::new(77),
        ));
        candidate.message_reference.as_mut().unwrap().message_id = Some(MessageId::new(88));
        candidate.kind = MessageType::InlineReply;
        assert_eq!(
            offer(&ingress, &candidate, MessageOccurrence::Created),
            DispatchOutcome::Offered
        );
        let observation = receiver.try_recv().unwrap();
        assert_eq!(
            observation.native.reference().unwrap().channel_id,
            ChannelId::new(77)
        );
        assert_eq!(observation.common.context().content(), candidate.content);
        assert!(config().channel_policy(77).is_none());
    }

    #[test]
    fn sdk_reference_null_limitation_is_explicit() {
        let absent = update();
        let explicit_null: MessageUpdateEvent = serde_json::from_value(serde_json::json!({"id":"30", "channel_id":"10", "guild_id":"40", "edited_timestamp":"2026-10-13T12:00:01.123456789Z", "message_reference":null})).unwrap();
        assert!(absent.message_reference.is_none());
        assert!(explicit_null.message_reference.is_none());
    }
    fn no_verification(_: Message) -> std::future::Ready<Option<VerifiedAppAction<Resolved>>> {
        std::future::ready(None)
    }

    #[tokio::test]
    async fn enabled_orchestration_hydrates_ambiguous_cached_reference_at_exact_revision() {
        let (ingress, mut receiver, _) = enabled();
        let cfg = Arc::new(config());
        let event = update();
        let mut old = message();
        old.message_reference = Some(MessageReference::new(
            MessageReferenceKind::Default,
            ChannelId::new(77),
        ));
        let mut cached = old.clone();
        event.apply_to_message(&mut cached);
        assert_eq!(cached.edited_timestamp, event.edited_timestamp);
        assert!(cached.message_reference.is_some());
        let mut authoritative = cached.clone();
        authoritative.message_reference = None;
        let reads = std::sync::atomic::AtomicUsize::new(0);
        let result = ingress
            .edit_with(
                &event,
                Some(&old),
                Some(&cached),
                99,
                || cfg.clone(),
                std::future::ready(None),
                async {
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(authoritative)
                },
                no_verification,
            )
            .await;
        assert_eq!(result.outcome, DispatchOutcome::Offered);
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(receiver.try_recv().unwrap().native.reference().is_none());
        let result = ingress
            .edit_with(
                &event,
                Some(&old),
                Some(&cached),
                99,
                || cfg.clone(),
                std::future::ready(None),
                std::future::ready(Ok(cached.clone())),
                no_verification,
            )
            .await;
        assert_eq!(result.outcome, DispatchOutcome::Offered);
        assert!(receiver.try_recv().unwrap().native.reference().is_some());
        let mut later = cached.clone();
        later.edited_timestamp =
            Some(serenity::model::Timestamp::parse("2026-10-13T12:00:03Z").unwrap());
        let result = ingress
            .edit_with(
                &event,
                Some(&old),
                Some(&cached),
                99,
                || cfg.clone(),
                std::future::ready(None),
                std::future::ready(Ok(later)),
                no_verification,
            )
            .await;
        assert_eq!(
            result.outcome,
            DispatchOutcome::Incomplete(ObservationFailure::RevisionMismatch)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn whole_orchestration_is_bounded_and_cheap_exclusions_skip_io() {
        let (ingress, _receiver, _) = enabled();
        let cfg = Arc::new(config());
        let mut candidate = message();
        let polls = std::sync::atomic::AtomicUsize::new(0);
        candidate.guild_id = Some(GuildId::new(41));
        let excluded = ingress
            .create_with(
                &candidate,
                99,
                || cfg.clone(),
                async {
                    polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    None
                },
                no_verification,
            )
            .await;
        assert_eq!(
            excluded.outcome,
            DispatchOutcome::Filtered(Exclusion::Scope)
        );
        candidate = message();
        candidate.author.id = UserId::new(90);
        let excluded = ingress
            .create_with(
                &candidate,
                99,
                || cfg.clone(),
                async {
                    polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    None
                },
                no_verification,
            )
            .await;
        assert_eq!(
            excluded.outcome,
            DispatchOutcome::Filtered(Exclusion::SelfMessage)
        );
        assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 0);
        let timed = ingress
            .create_with(
                &message(),
                99,
                || cfg.clone(),
                std::future::pending(),
                no_verification,
            )
            .await;
        assert_eq!(
            timed.outcome,
            DispatchOutcome::Incomplete(ObservationFailure::Timeout)
        );
        assert!(matches!(timed.admission, PreparedAdmission::Unobserved));
        let _permits = ingress
            .enabled
            .as_ref()
            .unwrap()
            .preprocessing
            .acquire_many(MAX_CONCURRENT_OBSERVATIONS as u32)
            .await
            .unwrap();
        let busy = ingress
            .create_with(
                &message(),
                99,
                || cfg.clone(),
                async {
                    polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    None
                },
                no_verification,
            )
            .await;
        assert_eq!(
            busy.outcome,
            DispatchOutcome::Incomplete(ObservationFailure::Busy)
        );
        assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn full_closed_and_disabled_enabled_orchestration_skip_all_io() {
        let (ingress, receiver, _) = enabled();
        let cfg = Arc::new(config());
        for _ in 0..4 {
            assert_eq!(
                ingress
                    .create_with(
                        &message(),
                        99,
                        || cfg.clone(),
                        std::future::ready(None),
                        no_verification
                    )
                    .await
                    .outcome,
                DispatchOutcome::Offered
            );
        }
        let polls = std::sync::atomic::AtomicUsize::new(0);
        assert_eq!(
            ingress
                .create_with(
                    &message(),
                    99,
                    || cfg.clone(),
                    async {
                        polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        None
                    },
                    no_verification
                )
                .await
                .outcome,
            DispatchOutcome::Full
        );
        drop(receiver);
        assert_eq!(
            ingress
                .create_with(
                    &message(),
                    99,
                    || cfg.clone(),
                    async {
                        polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        None
                    },
                    no_verification
                )
                .await
                .outcome,
            DispatchOutcome::SinkUnavailable
        );
        assert_eq!(
            ObservationIngress::disabled(source())
                .create_with(
                    &message(),
                    99,
                    || cfg.clone(),
                    async {
                        polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        None
                    },
                    no_verification
                )
                .await
                .outcome,
            DispatchOutcome::Disabled
        );
        assert_eq!(polls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn exact_thread_denial_overrides_parent_and_child_source_sender_policy_wins() {
        let (ingress, mut receiver, policy) = enabled();
        let cfg = Arc::new(config());
        policy.write().unwrap().rooms[0].include_threads = true;
        policy
            .write()
            .unwrap()
            .denied_rooms
            .insert((GuildId::new(40), ChannelId::new(11)));
        let mut child = message();
        child.channel_id = ChannelId::new(11);
        let denied = ingress
            .create_with(
                &child,
                99,
                || cfg.clone(),
                async { panic!("exact denied child must not resolve topology") },
                no_verification,
            )
            .await;
        assert_eq!(denied.outcome, DispatchOutcome::Filtered(Exclusion::Scope));
        child.channel_id = ChannelId::new(12);
        assert_eq!(
            ingress
                .create_with(
                    &child,
                    99,
                    || cfg.clone(),
                    std::future::ready(Some(10)),
                    no_verification
                )
                .await
                .outcome,
            DispatchOutcome::Offered
        );
        assert_eq!(
            receiver.try_recv().unwrap().thread_parent,
            Some(ChannelId::new(10))
        );
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "10".into(),
            allow_from: vec!["20".into()],
            ..Default::default()
        });
        raw.channels.push(ChannelConfig {
            id: "12".into(),
            allow_from: vec!["21".into()],
            ..Default::default()
        });
        let child_cfg = Arc::new(LoadedConfig::from_raw(raw));
        assert_eq!(
            ingress
                .create_with(
                    &child,
                    99,
                    || child_cfg.clone(),
                    std::future::ready(Some(10)),
                    no_verification
                )
                .await
                .outcome,
            DispatchOutcome::Filtered(Exclusion::Sender)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn recipient_and_source_revocation_after_wait_prevent_emission() {
        let (ingress, mut receiver, policy) = enabled();
        let cfg = Arc::new(config());
        let result = ingress
            .create_with(
                &message(),
                99,
                || cfg.clone(),
                async {
                    policy.write().unwrap().senders =
                        SenderFilter::Only(HashSet::from([UserId::new(21)]));
                    None
                },
                no_verification,
            )
            .await;
        assert_eq!(result.outcome, DispatchOutcome::Filtered(Exclusion::Sender));
        policy.write().unwrap().senders = SenderFilter::EveryoneInRoom;
        let snapshots = arc_swap::ArcSwap::from(cfg.clone());
        let result = ingress
            .create_with(
                &message(),
                99,
                || snapshots.load_full(),
                async {
                    snapshots.store(Arc::new(LoadedConfig::from_raw(Config::default())));
                    None
                },
                no_verification,
            )
            .await;
        assert_eq!(result.outcome, DispatchOutcome::Filtered(Exclusion::Scope));
        let event = update();
        let mut current = message();
        current.edited_timestamp = event.edited_timestamp;
        let result = ingress
            .edit_with(
                &event,
                None,
                None,
                99,
                || cfg.clone(),
                std::future::ready(None),
                async {
                    policy.write().unwrap().own_actors.insert(UserId::new(20));
                    Ok(current)
                },
                no_verification,
            )
            .await;
        assert_eq!(
            result.outcome,
            DispatchOutcome::Filtered(Exclusion::SelfMessage)
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn unknown_verification_and_generation_change_are_not_success() {
        let (ingress, mut receiver, _) = enabled();
        let cfg = Arc::new(config());
        let mut webhook = message();
        webhook.webhook_id = Some(WebhookId::new(55));
        let result = ingress
            .create_with(
                &webhook,
                99,
                || cfg.clone(),
                std::future::ready(None),
                no_verification,
            )
            .await;
        assert_eq!(
            result.outcome,
            DispatchOutcome::Incomplete(ObservationFailure::UnverifiedTransport)
        );
        let snapshots = arc_swap::ArcSwap::from(cfg);
        let result = ingress
            .create_with(
                &webhook,
                99,
                || snapshots.load_full(),
                std::future::ready(None),
                |_message| async {
                    snapshots.store(Arc::new(config()));
                    None
                },
            )
            .await;
        assert_eq!(
            result.outcome,
            DispatchOutcome::Incomplete(ObservationFailure::PolicyUnavailable)
        );
        assert!(matches!(result.admission, PreparedAdmission::Rejected));
        assert!(receiver.try_recv().is_err());
    }
}
