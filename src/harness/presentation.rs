//! Compact host presentation of scoped delivery observations.

use crate::{
    attention::delivery::AttentionDirective,
    core::{
        ActorRef, ConversationRef, MessageRef,
        delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext},
        events::{
            ControlEvent, CoordinationNotice, MessageCreated, ObservedText, OrdinaryEvent,
            PermissionDecision,
        },
    },
    evidence::locators::SentexRole,
    timestamp::format_compact,
};
use serde::{Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub(crate) enum Presentation {
    Channel(ChannelPresentation),
    Permission {
        request: String,
        decision: PermissionDecision,
    },
}
#[derive(Debug, Serialize)]
pub(crate) struct ChannelPresentation {
    pub(crate) content: String,
    pub(crate) meta: BTreeMap<&'static str, MetadataValue>,
}
#[derive(Debug)]
pub(crate) enum MetadataValue {
    Text(String),
    Flag(bool),
    Locators(Vec<LocatorProjection>),
}
#[derive(Debug, Serialize)]
pub(crate) struct LocatorProjection {
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
            crate::core::identity_display::format_identity(&author.local().to_string(), Some(name)),
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

pub(crate) fn project(delivery: &Delivery) -> Presentation {
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
            content.push_str(&crate::core::identity_display::format_identity(
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
mod single_tests {
    use crate::{
        bell_rings::BellStatus,
        discord::events::{MessageEvent, MessageTargeting, NotificationEvent},
        harness::test_support::FixtureNotification,
        timestamp::Timestamp,
    };
    use serde_json::json;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    #[test]
    fn test_message_edit_includes_thread_parent_id() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited text".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: Some(ChannelId::new(400)),
            reply_to_message_id: None,
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "400");
        assert_eq!(meta["type"], "message_edit");
    }

    #[test]
    fn test_message_edit_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited text".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn message_edit_projects_legacy_evidence_as_citations_and_removal_clears_it() {
        let edit = |new_content: &str| NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: new_content.into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let added = edit("edited [🔍=v1:AAAAAAAAAAw]").fixture_notification();
        assert_eq!(
            added["params"]["meta"]["citation_locators"],
            json!([{
                "locator": "v1:AAAAAAAAAAw",
                "author_id": "300",
            }])
        );
        assert!(added["params"]["meta"].get("claim_locators").is_none());

        let removed = edit("edited").fixture_notification();
        assert!(removed["params"]["meta"].get("claim_locators").is_none());
        assert!(removed["params"]["meta"].get("citation_locators").is_none());
    }

    #[test]
    fn disabled_evidence_projection_preserves_visible_content_only() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited [🔍=v1:AAAAAAAAAAw]".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let json = event.fixture_notification_with_evidence(false);
        assert_eq!(json["params"]["content"], "edited [🔍=v1:AAAAAAAAAAw]");
        assert!(json["params"]["meta"].get("evidence").is_none());
        assert!(json["params"]["meta"].get("claim_locators").is_none());
        assert!(json["params"]["meta"].get("citation_locators").is_none());
    }

    #[test]
    fn test_message_delete_includes_thread_parent_id() {
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            thread_parent_id: Some(ChannelId::new(500)),
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "500");
        assert_eq!(meta["type"], "message_delete");
    }

    #[test]
    fn test_message_delete_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            thread_parent_id: None,
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_omits_thread_parent_id_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "bob".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hello from channel".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("thread_parent_id").is_none());
    }

    #[test]
    fn test_message_includes_thread_parent_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "bob".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hello from thread".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: Some(ChannelId::new(600)),
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["thread_parent_id"], "600");
    }

    // ── Reply-to-message-id tests ─────────────────────────────────────────────

    #[test]
    fn test_message_includes_reply_to_message_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "replying to you".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "999");
    }

    #[test]
    fn test_message_omits_reply_to_message_id_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "not a reply".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_message_id").is_none());
    }

    #[test]
    fn test_message_includes_reply_context() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "replying to you".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: Some(UserId::new(888)),
            reply_to_user: Some("bob".into()),
            reply_to_content_preview: Some("original message".into()),
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_user_id"], "888");
        assert_eq!(meta["reply_to_user"], "bob");
        assert_eq!(meta["reply_to_content_preview"], "original message");
    }

    #[test]
    fn test_message_omits_reply_context_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "not a reply".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_user_id").is_none());
        assert!(meta.get("reply_to_user").is_none());
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_omits_reply_context_but_keeps_reply_to_message_id() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "reply without hydrated parent".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "999");
        assert!(meta.get("reply_to_user_id").is_none());
        assert!(meta.get("reply_to_user").is_none());
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_includes_bells_when_present() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hello with bells".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: Some("3s lain/person-pace;2b lain/feedback-no-platitudes".into()),
            bells_status: Some(BellStatus::Ok),
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(
            meta["bells"],
            "3s lain/person-pace;2b lain/feedback-no-platitudes"
        );
        assert_eq!(meta["bells_status"], "ok");
    }

    #[test]
    fn test_message_omits_bells_when_none() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hello without bells".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("bells").is_none());
    }

    #[test]
    fn test_message_omits_preview_but_keeps_author() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "reply to attachment-only parent".into(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(999)),
            reply_to_user_id: Some(UserId::new(888)),
            reply_to_user: Some("bob".into()),
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_user_id"], "888");
        assert_eq!(meta["reply_to_user"], "bob");
        assert!(meta.get("reply_to_content_preview").is_none());
    }

    #[test]
    fn test_message_edit_includes_reply_to_message_id() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited reply".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(888)),
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["reply_to_message_id"], "888");
    }

    #[test]
    fn test_message_edit_omits_reply_to_message_id_when_none() {
        let event = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(300),
            new_content: "edited non-reply".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert!(meta.get("reply_to_message_id").is_none());
    }

    #[test]
    fn message_envelope_has_no_application_transport_tells() {
        let event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(200),
            user: "alice".into(),
            user_id: UserId::new(4242),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hello".into(),
            targeting: MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        });
        let notification = event.fixture_notification();
        let meta = &notification["params"]["meta"];
        assert_eq!(meta["user_id"], "4242");
        for forbidden in [
            "app_action_provenance",
            "app_action_state",
            "app_action_provider",
            "represented_sender_id",
        ] {
            assert!(meta.get(forbidden).is_none(), "leaked {forbidden}");
        }
    }

    #[test]
    fn coordination_done_event_serializes_with_metadata() {
        let event = NotificationEvent::Coordination(crate::coordination::CoordinationEvent::Done {
            message_id: "7".parse().unwrap(),
            channel_id: "42".parse().unwrap(),
            bot_id: "111".parse().unwrap(),
            reply_message_id: "99".parse().unwrap(),
        });
        let json = event.fixture_notification();
        assert_eq!(json["method"], "notifications/claude/channel");
        let meta = &json["params"]["meta"];
        assert_eq!(meta["type"], "coordination");
        assert_eq!(meta["event"], "done");
        assert_eq!(meta["message_id"], "7");
        assert_eq!(meta["chat_id"], "42");
        assert_eq!(meta["by"], "111");
        assert_eq!(meta["reply_message_id"], "99");
    }

    #[test]
    fn coordination_promoted_event_serializes_without_reply_fields() {
        let event =
            NotificationEvent::Coordination(crate::coordination::CoordinationEvent::Promoted {
                message_id: "7".parse().unwrap(),
                channel_id: "42".parse().unwrap(),
            });
        let json = event.fixture_notification();
        let meta = &json["params"]["meta"];
        assert_eq!(meta["type"], "coordination");
        assert_eq!(meta["event"], "promoted");
        assert_eq!(meta["message_id"], "7");
        assert_eq!(meta["chat_id"], "42");
        assert!(meta.get("by").is_none());
        assert!(meta.get("reply_message_id").is_none());
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod batch_tests;
#[cfg(test)]
#[path = "compact_tests.rs"]
mod compact_tests;
