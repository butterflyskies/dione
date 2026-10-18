//! Reads the historical Discord notification format without rewriting payloads.

use crate::{
    attention::{
        delivery::{AttentionDirective, OwnedAttentionDescriptor},
        types::{RecordId, RecordIdError},
    },
    core::{
        ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef,
        ModuleInstanceId,
        delivery::DeliveryKind,
        events::{ControlEvent, CoordinationNotice, OrdinaryEvent},
    },
};
use serde_json::Value;
use std::num::NonZeroU64;
use thiserror::Error;

/// The explicit configured owner of legacy inbox payloads that predate module identities.
#[derive(Debug, Clone)]
pub struct LegacyDiscordScope(ModuleInstanceId);

impl LegacyDiscordScope {
    /// Assigns historical payloads to the configured Discord module instance.
    pub fn new(module: ModuleInstanceId) -> Self {
        Self(module)
    }
    pub(crate) fn module(&self) -> &ModuleInstanceId {
        &self.0
    }
}

/// A malformed managed payload; its original bytes are not changed.
#[derive(Debug, Error)]
#[error("invalid legacy attention delivery: {kind}")]
pub struct LegacyDeliveryError {
    #[source]
    kind: LegacyDeliveryErrorKind,
}

#[derive(Debug, Error)]
enum LegacyDeliveryErrorKind {
    #[error("missing or malformed {0}")]
    Field(&'static str),
    #[error("attention markers require a message create event")]
    EventKind,
    #[error(transparent)]
    Record(#[from] RecordIdError),
}

fn field(name: &'static str) -> LegacyDeliveryError {
    LegacyDeliveryError {
        kind: LegacyDeliveryErrorKind::Field(name),
    }
}

pub(crate) fn decode_attention(
    payload: &Value,
    scope: &LegacyDiscordScope,
) -> Result<Option<OwnedAttentionDescriptor>, LegacyDeliveryError> {
    let Some(meta) = payload.pointer("/params/meta").and_then(Value::as_object) else {
        return Ok(None);
    };
    let delivery = meta.get("attention_delivery");
    let record = meta.get("attention_record");
    if delivery.is_none() && record.is_none() {
        return Ok(None);
    }
    if let Some(method) = payload.get("method")
        && method.as_str() != Some("notifications/claude/channel")
    {
        return Err(field("channel notification method"));
    }
    let directive = match delivery.and_then(Value::as_str) {
        Some("prompt") => AttentionDirective::Prompt,
        Some("next_turn") => AttentionDirective::NextTurn,
        _ => return Err(field("attention_delivery")),
    };
    let record = RecordId::new(
        record
            .and_then(Value::as_str)
            .ok_or_else(|| field("attention_record"))?,
    )
    .map_err(|error| LegacyDeliveryError {
        kind: LegacyDeliveryErrorKind::Record(error),
    })?;
    if let Some(kind) = meta.get("type")
        && kind.as_str() != Some("message")
    {
        return Err(LegacyDeliveryError {
            kind: LegacyDeliveryErrorKind::EventKind,
        });
    }
    let numeric = |name: &'static str| -> Result<NonZeroU64, LegacyDeliveryError> {
        meta.get(name)
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u64>().ok())
            .and_then(NonZeroU64::new)
            .ok_or_else(|| field(name))
    };
    let source = MessageRef::new(
        ConversationRef::new(
            scope.0.clone(),
            ConversationId::numeric(numeric("chat_id")?),
        ),
        MessageId::numeric(numeric("message_id")?),
    );
    let author = ActorRef::new(scope.0.clone(), ActorId::numeric(numeric("user_id")?));
    let original_text = payload
        .pointer("/params/content")
        .and_then(Value::as_str)
        .ok_or_else(|| field("content"))?
        .to_owned();
    Ok(Some(OwnedAttentionDescriptor::new(
        record,
        directive,
        source,
        author,
        original_text,
    )))
}

pub(crate) fn encode(
    delivery: &crate::core::delivery::Delivery,
    scope: &LegacyDiscordScope,
) -> Result<Value, LegacyDeliveryError> {
    validate_delivery(delivery, scope)?;
    Ok(match crate::harness::presentation::project(delivery) {
        crate::harness::presentation::Presentation::Channel(channel) => {
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":channel.content,"meta":channel.meta}})
        }
        crate::harness::presentation::Presentation::Permission { request, decision } => {
            serde_json::json!({"jsonrpc":"2.0","method":"notifications/claude/channel/permission","params":{"request_id":request,"behavior":match decision {crate::core::events::PermissionDecision::Granted=>"allow",crate::core::events::PermissionDecision::Denied=>"deny"}}})
        }
    })
}

fn validate_delivery(
    delivery: &crate::core::delivery::Delivery,
    scope: &LegacyDiscordScope,
) -> Result<(), LegacyDeliveryError> {
    match delivery.kind() {
        DeliveryKind::Ordinary { first, rest } => {
            for event in std::iter::once(first).chain(rest) {
                validate_event(event, scope)?;
            }
        }
        DeliveryKind::Managed { message, .. } => validate_message(message, scope)?,
        DeliveryKind::Control(ControlEvent::Coordination(notice)) => match notice.as_ref() {
            CoordinationNotice::Done {
                claimed,
                responder,
                reply,
            } => {
                validate_source(claimed, scope)?;
                validate_actor(responder, scope)?;
                validate_source(reply, scope)?;
                if claimed.conversation() != reply.conversation() {
                    return Err(field("coordination reply conversation"));
                }
            }
            CoordinationNotice::Promoted { claimed } => validate_source(claimed, scope)?,
        },
        DeliveryKind::Control(
            ControlEvent::Permission { .. }
            | ControlEvent::Trace { .. }
            | ControlEvent::ConfigurationRejected { .. },
        ) => {}
    }
    Ok(())
}
fn validate_conversation(
    conversation: &ConversationRef,
    scope: &LegacyDiscordScope,
) -> Result<(), LegacyDeliveryError> {
    if conversation.module() != scope.module() || conversation.local().as_numeric().is_none() {
        Err(field("legacy conversation scope"))
    } else {
        Ok(())
    }
}
fn validate_source(
    source: &MessageRef,
    scope: &LegacyDiscordScope,
) -> Result<(), LegacyDeliveryError> {
    validate_conversation(source.conversation(), scope)?;
    if source.local().as_numeric().is_none() {
        Err(field("legacy message identity"))
    } else {
        Ok(())
    }
}
fn validate_actor(actor: &ActorRef, scope: &LegacyDiscordScope) -> Result<(), LegacyDeliveryError> {
    if actor.module() != scope.module() || actor.local().as_numeric().is_none() {
        Err(field("legacy actor scope"))
    } else {
        Ok(())
    }
}
fn validate_message(
    message: &crate::core::events::MessageCreated,
    scope: &LegacyDiscordScope,
) -> Result<(), LegacyDeliveryError> {
    validate_source(message.source(), scope)?;
    validate_actor(message.author().actor(), scope)?;
    if let Some(parent) = message.thread_parent() {
        validate_conversation(parent, scope)?;
    }
    if let Some(target) = message.reply().target() {
        validate_source(target, scope)?;
        if target.conversation() != message.source().conversation() {
            return Err(field("reply conversation"));
        }
    }
    if let Some(author) = message.reply().author() {
        validate_actor(author, scope)?;
    }
    Ok(())
}
fn validate_event(
    event: &crate::core::events::OrdinaryEvent,
    scope: &LegacyDiscordScope,
) -> Result<(), LegacyDeliveryError> {
    validate_source(event.source(), scope)?;
    if let Some(parent) = event.thread_parent() {
        validate_conversation(parent, scope)?;
    }
    match event {
        OrdinaryEvent::Message(message) => validate_message(message, scope)?,
        OrdinaryEvent::MessageEdited(edit) => {
            validate_actor(edit.author(), scope)?;
            if let Some(target) = edit.reply_target() {
                validate_source(target, scope)?;
                if target.conversation() != event.source().conversation() {
                    return Err(field("reply conversation"));
                }
            }
        }
        OrdinaryEvent::ReactionAdded { author, .. } => validate_actor(author, scope)?,
        OrdinaryEvent::MessageDeleted { .. } => {}
    }
    Ok(())
}
