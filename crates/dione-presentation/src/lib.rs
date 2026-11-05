//! Compact host presentation of retained delivery observations.
//!
//! Projection preserves supplied observation values. It does not authenticate
//! identities, check current access, or establish delivery completion.
#![warn(missing_docs)]

use dione_contracts::{ActorRef, ConversationRef, MessageRef};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};
use {
    dione_attention::delivery::AttentionDirective,
    dione_evidence::locators::SentexRole,
    dione_messages::{
        delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext},
        events::{
            ControlEvent, CoordinationNotice, MessageCreated, ObservedText, OrdinaryEvent,
            PermissionDecision,
        },
        timestamp::format_compact,
    },
};

/// A channel observation or a separate permission notice.
#[derive(Debug)]
pub enum Presentation {
    /// Content and metadata for a channel observation.
    Channel(ChannelPresentation),
    /// A retained host permission decision, separate from channel content.
    Permission {
        /// The supplied permission request identity.
        request: String,
        /// The supplied permission decision.
        decision: PermissionDecision,
    },
}
/// Descriptive channel content and metadata, without authorization guarantees.
#[derive(Debug, Serialize)]
pub struct ChannelPresentation {
    /// The visible projected text.
    pub content: String,
    /// The projected metadata using the existing stable field names.
    pub meta: BTreeMap<&'static str, MetadataValue>,
}
/// A projected text, flag, or retained locator list.
#[derive(Debug)]
pub enum MetadataValue {
    /// A textual metadata value.
    Text(String),
    /// A boolean metadata value.
    Flag(bool),
    /// Retained evidence locators with their supplied authors.
    Locators(Vec<LocatorProjection>),
}
/// A serializable retained locator and its supplied author identity.
#[derive(Debug, Serialize)]
pub struct LocatorProjection {
    locator: String,
    author_id: String,
}
impl Serialize for MetadataValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(value) => value.serialize(serializer),
            Self::Flag(value) => value.serialize(serializer),
            Self::Locators(value) => value.serialize(serializer),
        }
    }
}
impl ChannelPresentation {
    fn new(content: String) -> Self {
        Self {
            content,
            meta: BTreeMap::new(),
        }
    }
    fn text(&mut self, key: &'static str, value: impl ToString) {
        self.meta
            .insert(key, MetadataValue::Text(value.to_string()));
    }
    fn flag(&mut self, key: &'static str, value: bool) {
        if value {
            self.meta.insert(key, MetadataValue::Flag(true));
        }
    }
    fn source(&mut self, source: &MessageRef) {
        self.text("chat_id", source.conversation().local());
        self.text("message_id", source.local());
    }
    fn actor(&mut self, author: &ActorRef, name: &str) {
        self.text("user_id", author.local());
        self.text(
            "user",
            dione_identity_display::format_identity(&author.local().to_string(), Some(name)),
        );
    }
    fn parent(&mut self, parent: Option<&ConversationRef>) {
        if let Some(parent) = parent {
            self.text("thread_parent_id", parent.local());
        }
    }
    fn evidence(
        &mut self,
        content: &ObservedText,
        author: &ActorRef,
        visibility: EvidenceVisibility,
    ) {
        if matches!(visibility, EvidenceVisibility::Disabled) {
            return;
        }
        for (key, role) in [
            ("claim_locators", SentexRole::Claim),
            ("citation_locators", SentexRole::Citation),
        ] {
            let locators: Vec<_> = content
                .locators()
                .iter()
                .filter(|locator| locator.role() == role)
                .map(|locator| LocatorProjection {
                    locator: locator.as_str().to_owned(),
                    author_id: author.local().to_string(),
                })
                .collect();
            if !locators.is_empty() {
                self.meta.insert(key, MetadataValue::Locators(locators));
            }
        }
    }
}

/// Formats a retained delivery without checking current authorization.
///
/// Evidence visibility controls retained locator metadata, not source access.
pub fn project(delivery: &Delivery) -> Presentation {
    let context = delivery.presentation();
    match delivery.kind() {
        DeliveryKind::Control(event) => control(event),
        DeliveryKind::Managed {
            message,
            directive,
            record,
        } => {
            let mut result = message_projection(message, context.evidence());
            result.text(
                "attention_delivery",
                match directive {
                    AttentionDirective::Prompt => "prompt",
                    AttentionDirective::NextTurn => "next_turn",
                },
            );
            result.text("attention_record", record.as_str());
            Presentation::Channel(result)
        }
        DeliveryKind::Ordinary { first, rest } if rest.is_empty() => {
            Presentation::Channel(single(first, context.evidence()))
        }
        DeliveryKind::Ordinary { first, rest } => {
            Presentation::Channel(batch(first, rest, context))
        }
    }
}
fn message_projection(
    message: &MessageCreated,
    evidence: EvidenceVisibility,
) -> ChannelPresentation {
    let mut result = ChannelPresentation::new(message.content().text().to_owned());
    result.source(message.source());
    result.actor(message.author().actor(), message.author().name());
    result.text("ts", message.occurred_at());
    result.flag("is_voice_message", message.is_voice());
    result.parent(message.thread_parent());
    let reply = message.reply();
    if let Some(target) = reply.target() {
        result.text("reply_to_message_id", target.local());
    }
    if let Some(author) = reply.author() {
        result.text("reply_to_user_id", author.local());
    }
    if let Some(name) = reply.name() {
        result.text("reply_to_user", name);
    }
    if let Some(preview) = reply.preview() {
        result.text("reply_to_content_preview", preview);
    }
    if !message.attachments().is_empty() {
        result.text("attachment_count", message.attachments().len());
        result.text(
            "attachments",
            message
                .attachments()
                .iter()
                .map(|attachment| {
                    format!(
                        "{} ({}, {}KB)",
                        attachment.name(),
                        attachment.media_type().unwrap_or("unknown"),
                        attachment.size().get() / 1024
                    )
                })
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    if let Some(text) = message.bells().text() {
        result.text("bells", text);
    }
    if let Some(status) = message.bells().status() {
        result.text("bells_status", status.as_str());
    }
    result.evidence(message.content(), message.author().actor(), evidence);
    result
}
fn single(event: &OrdinaryEvent, evidence: EvidenceVisibility) -> ChannelPresentation {
    match event {
        OrdinaryEvent::Message(message) => message_projection(message, evidence),
        OrdinaryEvent::MessageEdited(edit) => {
            let mut result = ChannelPresentation::new(edit.content().text().to_owned());
            result.source(edit.source());
            result.actor(edit.author(), edit.author_name());
            result.text("type", "message_edit");
            result.text("ts", edit.occurred_at());
            result.parent(edit.thread_parent());
            if let Some(target) = edit.reply_target() {
                result.text("reply_to_message_id", target.local());
            }
            result.evidence(edit.content(), edit.author(), evidence);
            result
        }
        OrdinaryEvent::MessageDeleted {
            source,
            thread_parent,
        } => {
            let mut result = ChannelPresentation::new("message deleted".to_owned());
            result.source(source);
            result.text("type", "message_delete");
            result.parent(thread_parent.as_ref());
            result
        }
        OrdinaryEvent::ReactionAdded {
            source,
            author,
            author_name,
            emoji,
            self_react,
        } => {
            let mut result = ChannelPresentation::new(format!("reacted with {emoji}"));
            result.source(source);
            result.actor(author, author_name);
            result.text("type", "reaction");
            result.text("emoji", emoji);
            result.flag("self_react", *self_react);
            result
        }
    }
}
fn control(event: &ControlEvent) -> Presentation {
    let result = match event {
        ControlEvent::Permission { request, decision } => {
            return Presentation::Permission {
                request: request.as_str().to_owned(),
                decision: *decision,
            };
        }
        ControlEvent::Trace {
            level,
            target,
            message,
            fields,
        } => {
            let content = if fields.is_empty() {
                message.clone()
            } else {
                format!(
                    "{message} {{ {} }}",
                    fields
                        .iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let mut result = ChannelPresentation::new(content);
            result.text("type", "trace");
            result.text("level", level.as_str());
            result.text("target", target);
            result
        }
        ControlEvent::ConfigurationRejected { display_error } => {
            let mut result = ChannelPresentation::new(format!(
                "config.toml parse error — running on last valid config. Error: {display_error}"
            ));
            result.text("type", "config_error");
            result
        }
        ControlEvent::Coordination(notice) => match notice.as_ref() {
            CoordinationNotice::Done {
                claimed,
                responder,
                reply,
            } => {
                let mut result = ChannelPresentation::new(format!(
                    "claim-once: {} replied to message {} in channel {} (reply {}); no reply needed",
                    responder.local(),
                    claimed.local(),
                    claimed.conversation().local(),
                    reply.local()
                ));
                result.source(claimed);
                result.text("type", "coordination");
                result.text("event", "done");
                result.text("by", responder.local());
                result.text("reply_message_id", reply.local());
                result
            }
            CoordinationNotice::Promoted { claimed } => {
                let mut result = ChannelPresentation::new(format!(
                    "claim-once: you may now reply to message {} in channel {}",
                    claimed.local(),
                    claimed.conversation().local()
                ));
                result.source(claimed);
                result.text("type", "coordination");
                result.text("event", "promoted");
                result
            }
        },
    };
    Presentation::Channel(result)
}

fn batch(
    first: &OrdinaryEvent,
    rest: &[OrdinaryEvent],
    context: PresentationContext,
) -> ChannelPresentation {
    let mut groups: BTreeMap<&ConversationRef, Vec<&OrdinaryEvent>> = BTreeMap::new();
    for event in std::iter::once(first).chain(rest) {
        groups
            .entry(event.source().conversation())
            .or_default()
            .push(event);
    }
    let mut rendered = groups.into_iter().filter_map(|(conversation, events)| {
        let (first, rest) = events.split_first()?;
        Some(group(conversation, first, rest, context))
    });
    // Insertion of the required first event makes this branch unreachable through Delivery.
    let Some(mut result) = rendered.next() else {
        return single(first, context.evidence());
    };
    let mut multiple = false;
    for next in rendered {
        multiple = true;
        result.content.push('\n');
        result.content.push_str(&next.content);
    }
    if multiple {
        result.text("format", "multi_channel");
    }
    result
}
fn group(
    conversation: &ConversationRef,
    first: &OrdinaryEvent,
    rest: &[&OrdinaryEvent],
    context: PresentationContext,
) -> ChannelPresentation {
    let events = std::iter::once(first).chain(rest.iter().copied());
    let all_messages = events
        .clone()
        .all(|event| matches!(event, OrdinaryEvent::Message(_)));
    let mut content = format!(
        "[{} ch={}",
        if all_messages { "batch" } else { "events" },
        conversation.local()
    );
    if let Some(parent) = events.clone().find_map(OrdinaryEvent::thread_parent) {
        content.push_str(&format!(" thread={}", parent.local()));
    }
    content.push_str(&format!(" n={}", rest.len() + 1));
    if let Some(timezone) = context.timezone() {
        content.push_str(&format!(" tz={timezone}"));
    }
    if all_messages {
        content.push_str(&format!(
            " latest={}",
            rest.last().copied().unwrap_or(first).source().local()
        ));
    }
    content.push_str("]\n");
    let mut roster = BTreeSet::new();
    for event in events.clone() {
        match event {
            OrdinaryEvent::Message(message) => {
                roster.insert((message.author().actor(), message.author().name()));
            }
            OrdinaryEvent::MessageEdited(edit) => {
                roster.insert((edit.author(), edit.author_name()));
            }
            OrdinaryEvent::ReactionAdded {
                author,
                author_name,
                ..
            } => {
                roster.insert((author, author_name.as_str()));
            }
            OrdinaryEvent::MessageDeleted { .. } => {}
        }
    }
    if !roster.is_empty() {
        content.push_str("[users");
        for (actor, name) in roster {
            content.push(' ');
            content.push_str(&dione_identity_display::format_identity(
                &actor.local().to_string(),
                Some(name),
            ));
        }
        content.push_str("]\n");
    }
    content.push('\n');
    for (index, event) in events.enumerate() {
        if index > 0 {
            content.push('\n');
        }
        match event {
            OrdinaryEvent::Message(message) => {
                let text = message.content().text().trim_end_matches('\n');
                content.push_str(&format!(
                    "{}|{}|{}|L={}",
                    message.source().local(),
                    format_compact(message.occurred_at(), context.timezone()),
                    message.author().name(),
                    text.lines().count()
                ));
                if let Some(reply) = message.reply().target() {
                    content.push_str(&format!("|>{}", reply.local()));
                }
                if !message.attachments().is_empty() {
                    content.push_str(&format!("|+{}", message.attachments().len()));
                }
                content.push('\n');
                if !text.is_empty() {
                    content.push_str(text);
                    content.push('\n');
                }
            }
            OrdinaryEvent::MessageEdited(edit) => {
                let text = edit.content().text().trim_end_matches('\n');
                content.push_str(&format!(
                    "!edit|{}|{}|{}|L={}\n",
                    edit.source().local(),
                    format_compact(edit.occurred_at(), context.timezone()),
                    edit.author_name(),
                    text.lines().count()
                ));
                if !text.is_empty() {
                    content.push_str(text);
                    content.push('\n');
                }
            }
            OrdinaryEvent::MessageDeleted { source, .. } => {
                content.push_str(&format!("!delete|{}\n", source.local()))
            }
            OrdinaryEvent::ReactionAdded {
                source,
                author_name,
                emoji,
                ..
            } => content.push_str(&format!(
                "!react|{}|{author_name}|{emoji}\n",
                source.local()
            )),
        }
    }
    let mut result = ChannelPresentation::new(content);
    result.text("chat_id", conversation.local());
    result.text("type", "batch");
    result.text(
        "format",
        if all_messages {
            "batch_v1"
        } else {
            "events_v1"
        },
    );
    result
}

#[cfg(test)]
mod batch_tests;
