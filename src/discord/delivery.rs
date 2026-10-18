//! Discord ingress conversion into scoped, typed live deliveries.

use super::events::{MessageEvent, MessageTargeting, NotificationEvent};
use crate::{
    attention::types::SourceAuthorKind,
    bell_rings,
    coordination::CoordinationEvent,
    core::{
        ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef,
        ModuleInstanceId,
        delivery::{Delivery, PresentationContext},
        events::{
            AuthorOrigin, BellObservation, BellStatus, ByteSize, ControlError, ControlEvent,
            CoordinationNotice, MessageCreated, MessageEdited, ObservedAttachment, ObservedAuthor,
            OrdinaryEvent, PermissionDecision, PermissionRequestId, ReplyObservation, Targeting,
        },
    },
    gate::MentionKind,
};
use serenity::model::id::{ChannelId, MessageId as DiscordMessageId, UserId};
use thiserror::Error;

pub(crate) enum ConvertedEvent {
    Ordinary(OrdinaryEvent),
    Control(ControlEvent),
}

fn conversation(module: &ModuleInstanceId, channel: ChannelId) -> ConversationRef {
    ConversationRef::new(module.clone(), ConversationId::numeric(channel.into()))
}
fn source(module: &ModuleInstanceId, channel: ChannelId, message: DiscordMessageId) -> MessageRef {
    MessageRef::new(
        conversation(module, channel),
        MessageId::numeric(message.into()),
    )
}
fn actor(module: &ModuleInstanceId, user: UserId) -> ActorRef {
    ActorRef::new(module.clone(), ActorId::numeric(user.into()))
}

pub(crate) fn message(module: &ModuleInstanceId, event: MessageEvent) -> MessageCreated {
    let origin = match event.author_kind {
        SourceAuthorKind::Unknown => AuthorOrigin::Unknown,
        SourceAuthorKind::DirectHuman => AuthorOrigin::DirectHuman,
        SourceAuthorKind::DirectBot => AuthorOrigin::DirectBot,
        SourceAuthorKind::VerifiedWebhook => AuthorOrigin::VerifiedWebhook,
    };
    let targeting = match event.targeting {
        MessageTargeting::DirectMessage => Targeting::DirectMessage,
        MessageTargeting::Ambient => Targeting::Ambient,
        MessageTargeting::GuildDirected(MentionKind::DirectMention) => Targeting::DirectMention,
        MessageTargeting::GuildDirected(MentionKind::ReplyToConstruct) => {
            Targeting::ReplyToConstruct
        }
        MessageTargeting::GuildDirected(MentionKind::ConfiguredPattern) => {
            Targeting::ConfiguredPattern
        }
    };
    let status = event.bells_status.map(|status| match status {
        bell_rings::BellStatus::Ok => BellStatus::Ok,
        bell_rings::BellStatus::PartialTimeout => BellStatus::PartialTimeout,
        bell_rings::BellStatus::PartialError => BellStatus::PartialError,
        bell_rings::BellStatus::Timeout => BellStatus::Timeout,
        bell_rings::BellStatus::Error => BellStatus::Error,
    });
    let reply = ReplyObservation::new(
        event
            .reply_to_message_id
            .map(|id| source(module, event.chat_id, id)),
        event.reply_to_user_id.map(|id| actor(module, id)),
        event.reply_to_user,
        event.reply_to_content_preview,
    );
    MessageCreated::new(
        source(module, event.chat_id, event.message_id),
        ObservedAuthor::new(actor(module, event.user_id), event.user, origin),
        event.content,
        targeting,
        event.timestamp,
    )
    .with_attachments(
        event
            .attachments
            .into_iter()
            .map(|attachment| {
                ObservedAttachment::new(
                    attachment.name,
                    attachment.content_type,
                    ByteSize::new(attachment.size),
                )
            })
            .collect(),
        event.is_voice_message,
    )
    .with_context(
        event.thread_parent_id.map(|id| conversation(module, id)),
        reply,
    )
    .with_bells(BellObservation::new(event.bells, status))
}

pub(crate) fn convert(
    module: &ModuleInstanceId,
    event: NotificationEvent,
) -> Result<ConvertedEvent, DeliveryConversionError> {
    Ok(match event {
        NotificationEvent::Message(event) => {
            ConvertedEvent::Ordinary(OrdinaryEvent::Message(Box::new(message(module, event))))
        }
        NotificationEvent::MessageEdit {
            chat_id,
            message_id,
            user,
            user_id,
            new_content,
            timestamp,
            thread_parent_id,
            reply_to_message_id,
        } => ConvertedEvent::Ordinary(OrdinaryEvent::MessageEdited(Box::new(
            MessageEdited::new(
                source(module, chat_id, message_id),
                actor(module, user_id),
                user,
                new_content,
                timestamp,
            )
            .with_context(
                thread_parent_id.map(|id| conversation(module, id)),
                reply_to_message_id.map(|id| source(module, chat_id, id)),
            ),
        ))),
        NotificationEvent::MessageDelete {
            chat_id,
            message_id,
            thread_parent_id,
        } => ConvertedEvent::Ordinary(OrdinaryEvent::MessageDeleted {
            source: source(module, chat_id, message_id),
            thread_parent: thread_parent_id.map(|id| conversation(module, id)),
        }),
        NotificationEvent::Reaction {
            chat_id,
            message_id,
            user,
            user_id,
            emoji,
            self_react,
        } => ConvertedEvent::Ordinary(OrdinaryEvent::ReactionAdded {
            source: source(module, chat_id, message_id),
            author: actor(module, user_id),
            author_name: user,
            emoji,
            self_react,
        }),
        NotificationEvent::PermissionResponse {
            request_id,
            granted,
        } => ConvertedEvent::Control(ControlEvent::Permission {
            request: PermissionRequestId::new(request_id).map_err(control_error)?,
            decision: if granted {
                PermissionDecision::Granted
            } else {
                PermissionDecision::Denied
            },
        }),
        NotificationEvent::Trace {
            level,
            target,
            message,
            fields,
        } => ConvertedEvent::Control(ControlEvent::Trace {
            level,
            target,
            message,
            fields,
        }),
        NotificationEvent::ConfigError { error } => {
            ConvertedEvent::Control(ControlEvent::ConfigurationRejected {
                display_error: error,
            })
        }
        NotificationEvent::Coordination(event) => {
            let channel = ConversationRef::new(
                module.clone(),
                ConversationId::numeric(event.channel_id().into()),
            );
            let claimed = MessageRef::new(
                channel.clone(),
                MessageId::numeric(event.message_id().into()),
            );
            let notice = match event {
                CoordinationEvent::Done {
                    bot_id,
                    reply_message_id,
                    ..
                } => CoordinationNotice::Done {
                    claimed,
                    responder: ActorRef::new(module.clone(), ActorId::numeric(bot_id.into())),
                    reply: MessageRef::new(channel, MessageId::numeric(reply_message_id.into())),
                },
                CoordinationEvent::Promoted { .. } => CoordinationNotice::Promoted { claimed },
            };
            ConvertedEvent::Control(ControlEvent::Coordination(Box::new(notice)))
        }
    })
}

pub(crate) fn single(
    module: &ModuleInstanceId,
    event: NotificationEvent,
    presentation: PresentationContext,
) -> Result<Delivery, DeliveryConversionError> {
    Ok(match convert(module, event)? {
        ConvertedEvent::Ordinary(event) => Delivery::single(event, presentation),
        ConvertedEvent::Control(event) => Delivery::control(event, presentation),
    })
}

pub(crate) fn batch(
    module: &ModuleInstanceId,
    first: NotificationEvent,
    rest: Vec<NotificationEvent>,
    presentation: PresentationContext,
) -> Result<Delivery, DeliveryConversionError> {
    if rest.is_empty() {
        return single(module, first, presentation);
    }
    let ordinary = |event| match convert(module, event)? {
        ConvertedEvent::Ordinary(event) => Ok(event),
        ConvertedEvent::Control(_) => Err(DeliveryConversionError {
            kind: DeliveryConversionErrorKind::MixedControl,
        }),
    };
    let first = ordinary(first)?;
    let rest = rest
        .into_iter()
        .map(ordinary)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Delivery::batch(first, rest, presentation))
}

fn control_error(source: ControlError) -> DeliveryConversionError {
    DeliveryConversionError {
        kind: DeliveryConversionErrorKind::Control(source),
    }
}
#[derive(Debug, Error)]
#[error("cannot convert Discord delivery: {kind}")]
pub(crate) struct DeliveryConversionError {
    #[source]
    kind: DeliveryConversionErrorKind,
}
#[derive(Debug, Error)]
enum DeliveryConversionErrorKind {
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("a control event cannot be included in an ordinary batch")]
    MixedControl,
}
#[cfg(test)]
#[path = "delivery_tests.rs"]
mod tests;
