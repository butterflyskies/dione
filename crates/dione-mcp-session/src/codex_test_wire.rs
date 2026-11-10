use dione_contracts::{ActorRef, ConversationRef, LocalIdentityRef, MessageRef};
use dione_messages::{
    delivery::{Delivery, DeliveryKind, EvidenceVisibility},
    events::{AuthorOrigin, BellStatus, OrdinaryEvent, Targeting},
};
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
