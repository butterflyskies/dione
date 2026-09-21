//! Bounded live source recovery. Metadata is not proof that a source still exists.

use super::{
    provider::SourceExcerpt,
    types::{DirectAuthorKind, SourceAuthorKind, SourceKey, SourceVersion, content_hash},
};
use crate::{
    config::load_config,
    discord::{events::MessageTargeting, verified_action::LifecycleContext},
    gate::{GateDecision, InboundGate, OutboundGate},
    ingress_ledger::IngressLedger,
    state::State,
};
use camino::Utf8PathBuf;
use serenity::{http::Http, model::channel::Channel};
use std::{sync::Arc, time::Duration};

/// Failure to establish current source authority, exact content, or fresh evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SourceFailure {
    #[error("source access is unavailable under current policy")]
    Access,
    #[error("source was removed or its content version changed")]
    Invalidated,
    #[error("source could not be revalidated")]
    Unknown,
}

struct ResolvedSource {
    source: SourceVersion,
    message: serenity::model::channel::Message,
    thread_parent: Option<serenity::model::id::ChannelId>,
}

impl ResolvedSource {
    fn reply(&self) -> Option<SourceKey> {
        self.message
            .message_reference
            .as_ref()
            .and_then(|reference| {
                reference.message_id.map(|message_id| SourceKey {
                    channel_id: reference.channel_id,
                    message_id,
                })
            })
    }

    fn into_excerpt(self) -> SourceExcerpt {
        SourceExcerpt {
            source: self.source,
            text: self.message.content,
        }
    }
}

/// Rehydrates Discord sources under current access, bot, transport and provider-export policy.
#[derive(Clone)]
pub struct SourceResolver {
    pub http: Arc<Http>,
    pub state: State,
    pub state_dir: Utf8PathBuf,
    pub ledger: Arc<IngressLedger>,
}

impl SourceResolver {
    /// Fetches an exact source version; `for_provider` also requires provider export authority.
    pub async fn fetch(
        &self,
        source: &SourceVersion,
        for_provider: bool,
    ) -> Result<SourceExcerpt, SourceFailure> {
        self.resolve(source.key, Some(source), for_provider).await
    }

    /// Resolves authorized bytes, optionally requiring an exact previously observed version.
    pub async fn resolve(
        &self,
        key: SourceKey,
        expected: Option<&SourceVersion>,
        for_provider: bool,
    ) -> Result<SourceExcerpt, SourceFailure> {
        self.resolve_with_parent(key, expected, for_provider)
            .await
            .map(ResolvedSource::into_excerpt)
    }

    /// Returns the authorized trigger, bounded reply ancestors, and whether context was unavailable.
    pub async fn segment(
        &self,
        source: &SourceVersion,
        limit: usize,
    ) -> Result<(SourceExcerpt, Vec<SourceExcerpt>, bool), SourceFailure> {
        let resolved = self
            .resolve_with_parent(source.key, Some(source), true)
            .await?;
        let (antecedents, missing) = self.antecedents(source.key, resolved.reply(), limit).await;
        Ok((resolved.into_excerpt(), antecedents, missing))
    }

    async fn resolve_with_parent(
        &self,
        key: SourceKey,
        expected: Option<&SourceVersion>,
        for_provider: bool,
    ) -> Result<ResolvedSource, SourceFailure> {
        let config = load_config(&self.state_dir);
        // A known local denial dominates unavailable remote evidence. Do not
        // retain labels as merely unknown while waiting for Discord to answer.
        if expected.is_some_and(|source| {
            config.is_ignored(source.author_id.get())
                || (source.author_kind == SourceAuthorKind::DirectBot
                    && !config.is_allowed(source.author_id.get()))
        }) {
            return Err(SourceFailure::Access);
        }
        let attention = &config.raw.attention;
        attention.validate().map_err(|_| SourceFailure::Access)?;
        if for_provider && !attention.provider_eligible(key.channel_id) {
            return Err(SourceFailure::Access);
        }
        let timeout = Duration::from_millis(attention.request_timeout_ms);
        let channel = tokio::time::timeout(timeout, self.http.get_channel(key.channel_id))
            .await
            .map_err(|_| SourceFailure::Unknown)?
            .map_err(|_| SourceFailure::Unknown)?;
        if channel.id() != key.channel_id {
            return Err(SourceFailure::Unknown);
        }
        let (context, parent) = match channel {
            Channel::Guild(channel) => {
                use serenity::model::channel::ChannelType;
                let parent = if matches!(
                    channel.kind,
                    ChannelType::PublicThread
                        | ChannelType::PrivateThread
                        | ChannelType::NewsThread
                ) {
                    channel.parent_id
                } else {
                    None
                };
                (LifecycleContext::Guild(channel.guild_id), parent)
            }
            Channel::Private(_) => (LifecycleContext::DirectMessage, None),
            _ => return Err(SourceFailure::Unknown),
        };
        let current = load_config(&self.state_dir);
        let state = self.state.read().await;
        let channel_allowed = OutboundGate::check_channel_with_threads(
            &current,
            key.channel_id.get(),
            &state.dm_channel_ids,
            &state.thread_parents,
        ) || parent
            .is_some_and(|parent| current.channel_policy(parent.get()).is_some());
        drop(state);
        if !channel_allowed
            || (for_provider && !current.raw.attention.provider_eligible(key.channel_id))
        {
            return Err(SourceFailure::Access);
        }
        let message = tokio::time::timeout(
            timeout,
            self.http.get_message(key.channel_id, key.message_id),
        )
        .await
        .map_err(|_| SourceFailure::Unknown)?
        .map_err(|error| match &error {
            serenity::Error::Http(http)
                if http
                    .status_code()
                    .is_some_and(|status| status.as_u16() == 404) =>
            {
                SourceFailure::Invalidated
            }
            _ => SourceFailure::Unknown,
        })?;
        if message.id != key.message_id || message.channel_id != key.channel_id {
            return Err(SourceFailure::Unknown);
        }
        if for_provider && !message.attachments.is_empty() {
            return Err(SourceFailure::Unknown);
        }
        let hash = content_hash(&message.content);
        if expected.is_some_and(|source| source.content_hash != hash) {
            return Err(SourceFailure::Invalidated);
        }
        let current = load_config(&self.state_dir);
        let snapshot = self
            .ledger
            .active_snapshot(key.message_id, key.channel_id, context);
        if snapshot.is_none()
            && (message.webhook_id.is_some()
                || expected.is_some_and(|source| source.author_id != message.author.id))
        {
            return Err(SourceFailure::Unknown);
        }
        let (author, author_kind) = snapshot.as_ref().map_or_else(
            || {
                (
                    message.author.id,
                    SourceAuthorKind::from(DirectAuthorKind::from_bot_flag(message.author.bot)),
                )
            },
            |snapshot| (snapshot.effective_user_id(), snapshot.author_kind()),
        );
        if expected.is_some_and(|source| source.author_id != author) {
            return Err(SourceFailure::Invalidated);
        }
        if expected.is_some_and(|source| {
            source.author_kind != SourceAuthorKind::Unknown && source.author_kind != author_kind
        }) {
            return Err(SourceFailure::Invalidated);
        }
        if current.is_ignored(author.get())
            || (author_kind == SourceAuthorKind::DirectBot && !current.is_allowed(author.get()))
            || (for_provider && !current.raw.attention.provider_eligible(key.channel_id))
        {
            return Err(SourceFailure::Access);
        }
        // A restart loses proxy proof. Never turn a webhook transport identity into
        // a represented principal merely because a persisted row names one.
        let delivery = self.ledger.delivery_check(
            key.message_id,
            key.channel_id,
            &hash,
            &current,
            MessageTargeting::Ambient,
        );
        if delivery == Err(crate::ingress_ledger::SourceDeliveryFailure::Invalidated) {
            return Err(SourceFailure::Invalidated);
        }
        if delivery.is_err() {
            if message.webhook_id.is_some() {
                return Err(SourceFailure::Unknown);
            }
            let allowed = match context {
                LifecycleContext::DirectMessage => matches!(
                    InboundGate::check_dm(&current, author.get()),
                    GateDecision::Deliver
                ),
                LifecycleContext::Guild(guild) => {
                    let scope = if current.channel_policy(key.channel_id.get()).is_some() {
                        key.channel_id
                    } else {
                        parent.unwrap_or(key.channel_id)
                    };
                    matches!(
                        InboundGate::check_guild(
                            &current,
                            scope.get(),
                            author.get(),
                            false,
                            Some(guild.get())
                        ),
                        GateDecision::Deliver
                    )
                }
            };
            if !allowed {
                return Err(SourceFailure::Access);
            }
            self.ledger.admit_direct_create(
                key.message_id,
                key.channel_id,
                context,
                author,
                DirectAuthorKind::from_bot_flag(message.author.bot),
                parent,
                &message.content,
                message.edited_timestamp.unwrap_or(message.timestamp),
            );
        }
        let latest = load_config(&self.state_dir);
        self.ledger
            .delivery_check(
                key.message_id,
                key.channel_id,
                &hash,
                &latest,
                MessageTargeting::Ambient,
            )
            .map_err(|failure| match failure {
                crate::ingress_ledger::SourceDeliveryFailure::Unavailable => SourceFailure::Unknown,
                crate::ingress_ledger::SourceDeliveryFailure::Invalidated => {
                    SourceFailure::Invalidated
                }
            })?;
        if for_provider && !latest.raw.attention.provider_eligible(key.channel_id) {
            return Err(SourceFailure::Access);
        }
        let source = expected.map_or_else(
            || SourceVersion {
                key,
                author_id: author,
                author_kind,
                // Conservative channel/thread grouping cannot split a conversation
                // across holdouts, even when a reply parent is outside the working set.
                conversation: format!("channel:{}", key.channel_id),
                content_hash: hash,
                observed_at_ms: now_ms(),
            },
            |expected| {
                let mut source = expected.clone();
                if source.author_kind == SourceAuthorKind::Unknown {
                    source.author_kind = author_kind;
                }
                source
            },
        );
        Ok(ResolvedSource {
            source,
            message,
            thread_parent: parent,
        })
    }

    async fn antecedents(
        &self,
        trigger: SourceKey,
        parent: Option<SourceKey>,
        limit: usize,
    ) -> (Vec<SourceExcerpt>, bool) {
        let mut next = parent;
        let mut excerpts = Vec::with_capacity(limit.min(32));
        while let Some(key) = next {
            if key == trigger
                || key.channel_id != trigger.channel_id
                || excerpts.len() >= limit
                || excerpts
                    .iter()
                    .any(|excerpt: &SourceExcerpt| excerpt.source.key == key)
            {
                return (excerpts, true);
            }
            match self.resolve_with_parent(key, None, true).await {
                Ok(resolved) => {
                    next = resolved.reply();
                    excerpts.push(resolved.into_excerpt());
                }
                Err(_) => return (excerpts, true),
            }
        }
        (excerpts, false)
    }

    /// Reconstructs an ambient event after revalidating its exact source and current authority.
    pub async fn recover_event(
        &self,
        source: &SourceVersion,
    ) -> Result<crate::discord::events::MessageEvent, SourceFailure> {
        let resolved = self
            .resolve_with_parent(source.key, Some(source), false)
            .await?;
        let reply = resolved.reply();
        let message = resolved.message;
        let timestamp = load_config(&self.state_dir).localize_rfc3339(
            &message
                .timestamp
                .to_rfc3339()
                .ok_or(SourceFailure::Unknown)?,
        );
        Ok(crate::discord::events::MessageEvent {
            chat_id: source.key.channel_id,
            message_id: source.key.message_id,
            user: format!("<@{}>", source.author_id),
            user_id: source.author_id,
            author_kind: source.author_kind,
            content: message.content,
            targeting: MessageTargeting::Ambient,
            timestamp,
            attachments: message
                .attachments
                .into_iter()
                .map(|attachment| crate::discord::events::AttachmentMeta {
                    name: attachment.filename,
                    content_type: attachment.content_type,
                    size: u64::from(attachment.size),
                })
                .collect(),
            is_voice_message: message.flags.is_some_and(|flags| {
                flags.contains(serenity::model::channel::MessageFlags::IS_VOICE_MESSAGE)
            }),
            thread_parent_id: resolved.thread_parent,
            reply_to_message_id: reply
                .filter(|key| key.channel_id == source.key.channel_id)
                .map(|key| key.message_id),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }
}

/// Returns saturating Unix milliseconds, or zero when the clock predates the epoch.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}
