use dione_attention::delivery::{AttentionDescriptor, AttentionDirective};
use dione_contracts::{
    ActorId, ActorRef, ConversationId, ConversationRef, LocalIdentityRef, MessageId, MessageRef,
    ModuleInstanceId, ModuleSlotId, ProviderId,
};
use dione_messages::{
    delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext},
    events::{
        AuthorOrigin, BellObservation, BellStatus, ByteSize, MessageCreated, ObservedAttachment,
        ObservedAuthor, OrdinaryEvent, ReplyObservation, Targeting,
    },
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub(super) fn encode(delivery: &Delivery) -> Value {
    let DeliveryKind::Ordinary { first, rest } = delivery.kind() else {
        panic!("lost-tail fixture accepts ordinary observed messages only")
    };
    let messages: Vec<Value> = std::iter::once(first)
        .chain(rest)
        .map(|event| {
            let OrdinaryEvent::Message(message) = event else {
                panic!("lost-tail fixture accepts message creation observations only")
            };
            let origin = match message.author().origin() {
                AuthorOrigin::Unknown => "unknown",
                AuthorOrigin::DirectHuman => "direct_human",
                AuthorOrigin::DirectBot => "direct_bot",
                AuthorOrigin::VerifiedWebhook => "verified_webhook",
            };
            let targeting = match message.targeting() {
                Targeting::DirectMessage => "direct_message",
                Targeting::DirectMention => "direct_mention",
                Targeting::ReplyToConstruct => "reply_to_construct",
                Targeting::ConfiguredPattern => "configured_pattern",
                Targeting::Ambient => "ambient",
            };
            let attachments: Vec<Value> = message.attachments().iter().map(|attachment| {
                json!({"name":attachment.name(),"media_type":attachment.media_type(),"size":attachment.size().get()})
            }).collect();
            json!({
                "source":encode_message(message.source()), "actor":encode_actor(message.author().actor()),
                "author_name":message.author().name(), "origin":origin,
                "text":message.content().text(), "targeting":targeting,
                "timestamp":message.occurred_at(), "attachments":attachments,
                "voice":message.is_voice(), "thread_parent":message.thread_parent().map(encode_conversation),
                "reply_target":message.reply().target().map(encode_message), "reply_author":message.reply().author().map(encode_actor),
                "reply_name":message.reply().name(), "reply_preview":message.reply().preview(),
                "bells":message.bells().text(), "bells_status":message.bells().status().map(BellStatus::as_str)
            })
        })
        .collect();
    json!({
        "messages":messages,
        "timezone":delivery.presentation().timezone().map(|timezone| timezone.to_string()),
        "evidence":matches!(delivery.presentation().evidence(), EvidenceVisibility::Enabled)
    })
}

fn field<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap()
}

pub(super) fn decode(value: &Value) -> Delivery {
    let mut messages = value["messages"].as_array().unwrap().iter().map(|message| {
        let origin = match message["origin"].as_str().unwrap() {
            "unknown" => AuthorOrigin::Unknown,
            "direct_human" => AuthorOrigin::DirectHuman,
            "direct_bot" => AuthorOrigin::DirectBot,
            "verified_webhook" => AuthorOrigin::VerifiedWebhook,
            other => panic!("unexpected observed origin {other}"),
        };
        let targeting = match message["targeting"].as_str().unwrap() {
            "direct_message" => Targeting::DirectMessage,
            "direct_mention" => Targeting::DirectMention,
            "reply_to_construct" => Targeting::ReplyToConstruct,
            "configured_pattern" => Targeting::ConfiguredPattern,
            "ambient" => Targeting::Ambient,
            other => panic!("unexpected observed targeting {other}"),
        };
        let status = match message["bells_status"].as_str() {
            None => None,
            Some("ok") => Some(BellStatus::Ok),
            Some("partial_timeout") => Some(BellStatus::PartialTimeout),
            Some("partial_error") => Some(BellStatus::PartialError),
            Some("timeout") => Some(BellStatus::Timeout),
            Some("error") => Some(BellStatus::Error),
            other => panic!("unexpected observed bell status {other:?}"),
        };
        let attachments = message["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|attachment| {
                ObservedAttachment::new(
                    field(&attachment["name"]),
                    field(&attachment["media_type"]),
                    ByteSize::new(field(&attachment["size"])),
                )
            })
            .collect();
        let observed = MessageCreated::new(
            decode_message(&message["source"]),
            ObservedAuthor::new(
                decode_actor(&message["actor"]),
                field(&message["author_name"]),
                origin,
            ),
            field(&message["text"]),
            targeting,
            field(&message["timestamp"]),
        )
        .with_attachments(attachments, field(&message["voice"]))
        .with_context(
            optional(&message["thread_parent"], decode_conversation),
            ReplyObservation::new(
                optional(&message["reply_target"], decode_message),
                optional(&message["reply_author"], decode_actor),
                field(&message["reply_name"]),
                field(&message["reply_preview"]),
            ),
        )
        .with_bells(BellObservation::new(field(&message["bells"]), status));
        OrdinaryEvent::Message(Box::new(observed))
    });
    let presentation = PresentationContext::new(
        value["timezone"]
            .as_str()
            .map(|timezone| timezone.parse().unwrap()),
        if value["evidence"].as_bool().unwrap() {
            EvidenceVisibility::Enabled
        } else {
            EvidenceVisibility::Disabled
        },
    );
    Delivery::batch(
        messages.next().expect("required first observed message"),
        messages.collect(),
        presentation,
    )
}

fn encode_local(local: LocalIdentityRef<'_>) -> Value {
    match local {
        LocalIdentityRef::Numeric(value) => json!({"numeric":value.get()}),
        LocalIdentityRef::Opaque(value) => json!({"opaque":value}),
    }
}
fn encode_conversation(value: &ConversationRef) -> Value {
    json!({"provider":value.module().provider().as_str(),"slot":value.module().slot().as_str(),"local":encode_local(value.local().identity())})
}
fn encode_actor(value: &ActorRef) -> Value {
    json!({"provider":value.module().provider().as_str(),"slot":value.module().slot().as_str(),"local":encode_local(value.local().identity())})
}
fn encode_message(value: &MessageRef) -> Value {
    json!({"conversation":encode_conversation(value.conversation()),"local":encode_local(value.local().identity())})
}
fn module(value: &Value) -> ModuleInstanceId {
    ModuleInstanceId::new(
        ProviderId::new(value["provider"].as_str().unwrap()).unwrap(),
        ModuleSlotId::new(value["slot"].as_str().unwrap()).unwrap(),
    )
}
fn decode_conversation(value: &Value) -> ConversationRef {
    let local = if let Some(numeric) = value["local"]["numeric"].as_u64() {
        ConversationId::numeric(numeric.try_into().unwrap())
    } else {
        ConversationId::opaque(value["local"]["opaque"].as_str().unwrap()).unwrap()
    };
    ConversationRef::new(module(value), local)
}
fn decode_actor(value: &Value) -> ActorRef {
    let local = if let Some(numeric) = value["local"]["numeric"].as_u64() {
        ActorId::numeric(numeric.try_into().unwrap())
    } else {
        ActorId::opaque(value["local"]["opaque"].as_str().unwrap()).unwrap()
    };
    ActorRef::new(module(value), local)
}
fn decode_message(value: &Value) -> MessageRef {
    let local = if let Some(numeric) = value["local"]["numeric"].as_u64() {
        MessageId::numeric(numeric.try_into().unwrap())
    } else {
        MessageId::opaque(value["local"]["opaque"].as_str().unwrap()).unwrap()
    };
    MessageRef::new(decode_conversation(&value["conversation"]), local)
}
fn optional<T>(value: &Value, parse: impl FnOnce(&Value) -> T) -> Option<T> {
    if value.is_null() {
        None
    } else {
        Some(parse(value))
    }
}

pub(super) fn encode_descriptor(descriptor: AttentionDescriptor<'_>) -> Value {
    let directive = match descriptor.directive() {
        AttentionDirective::Prompt => "prompt",
        AttentionDirective::NextTurn => "next_turn",
    };
    json!({"record":descriptor.record().as_str(), "directive":directive,
        "source":encode_message(descriptor.source()), "author":encode_actor(descriptor.author()),
        "text":descriptor.original_text()})
}
