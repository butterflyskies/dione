use super::*;
use crate::{
    attention::{delivery::AttentionDirective, types::RecordId},
    core::delivery::DeliveryKind,
    discord::events::AttachmentMeta,
    harness::test_support,
    timestamp::Timestamp,
};

fn fixture() -> MessageEvent {
    MessageEvent {
        chat_id: ChannelId::new(10),
        message_id: DiscordMessageId::new(20),
        user: "same display (she/her)".into(),
        user_id: UserId::new(30),
        author_kind: SourceAuthorKind::VerifiedWebhook,
        content: "exact original\n\n".into(),
        targeting: MessageTargeting::GuildDirected(MentionKind::ConfiguredPattern),
        timestamp: Timestamp::parse("2026-01-01T01:02:03+02:00").unwrap(),
        attachments: [0, 1023, 1024, 1025]
            .into_iter()
            .map(|size| AttachmentMeta {
                name: format!("file-{size}"),
                content_type: Some("text/plain".into()),
                size,
            })
            .collect(),
        is_voice_message: true,
        thread_parent_id: Some(ChannelId::new(9)),
        reply_to_message_id: Some(DiscordMessageId::new(19)),
        reply_to_user_id: Some(UserId::new(29)),
        reply_to_user: Some("original author".into()),
        reply_to_content_preview: Some("preview\n".into()),
        bells: Some("bell text".into()),
        bells_status: Some(bell_rings::BellStatus::PartialTimeout),
    }
}

#[test]
fn conversion_retains_every_observed_create_field_and_exact_sizes() {
    let module = test_support::module();
    let converted = message(&module, fixture());
    assert_eq!(
        converted.source(),
        &source(&module, ChannelId::new(10), DiscordMessageId::new(20))
    );
    assert_eq!(converted.author().actor(), &actor(&module, UserId::new(30)));
    assert_eq!(converted.author().name(), "same display (she/her)");
    assert_eq!(converted.author().origin(), AuthorOrigin::VerifiedWebhook);
    assert_eq!(converted.content().text(), "exact original\n\n");
    assert_eq!(converted.targeting(), Targeting::ConfiguredPattern);
    assert_eq!(
        converted.occurred_at().to_rfc3339(),
        "2026-01-01T01:02:03+02:00"
    );
    assert_eq!(
        converted
            .attachments()
            .iter()
            .map(|attachment| (
                attachment.name(),
                attachment.media_type(),
                attachment.size().get()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("file-0", Some("text/plain"), 0),
            ("file-1023", Some("text/plain"), 1023),
            ("file-1024", Some("text/plain"), 1024),
            ("file-1025", Some("text/plain"), 1025)
        ]
    );
    assert!(converted.is_voice());
    assert_eq!(
        converted.thread_parent(),
        Some(&conversation(&module, ChannelId::new(9)))
    );
    assert_eq!(
        converted.reply().target(),
        Some(&source(
            &module,
            ChannelId::new(10),
            DiscordMessageId::new(19)
        ))
    );
    assert_eq!(
        converted.reply().author(),
        Some(&actor(&module, UserId::new(29)))
    );
    assert_eq!(converted.reply().name(), Some("original author"));
    assert_eq!(converted.reply().preview(), Some("preview\n"));
    assert_eq!(converted.bells().text(), Some("bell text"));
    assert_eq!(converted.bells().status(), Some(BellStatus::PartialTimeout));
}

#[test]
fn independent_reply_observations_and_attachment_only_text_remain_representable() {
    for mask in 0..16 {
        let mut event = fixture();
        event.content.clear();
        if mask & 1 == 0 {
            event.reply_to_message_id = None;
        }
        if mask & 2 == 0 {
            event.reply_to_user_id = None;
        }
        if mask & 4 == 0 {
            event.reply_to_user = None;
        }
        if mask & 8 == 0 {
            event.reply_to_content_preview = None;
        }
        let converted = message(&test_support::module(), event);
        assert_eq!(converted.content().text(), "");
        assert_eq!(converted.attachments().len(), 4);
        assert_eq!(converted.reply().target().is_some(), mask & 1 != 0);
        assert_eq!(converted.reply().author().is_some(), mask & 2 != 0);
        assert_eq!(converted.reply().name().is_some(), mask & 4 != 0);
        assert_eq!(converted.reply().preview().is_some(), mask & 8 != 0);
    }
}

#[test]
fn origin_targeting_and_bell_classes_convert_exhaustively() {
    for (raw, expected) in [
        (SourceAuthorKind::Unknown, AuthorOrigin::Unknown),
        (SourceAuthorKind::DirectHuman, AuthorOrigin::DirectHuman),
        (SourceAuthorKind::DirectBot, AuthorOrigin::DirectBot),
        (
            SourceAuthorKind::VerifiedWebhook,
            AuthorOrigin::VerifiedWebhook,
        ),
    ] {
        let mut event = fixture();
        event.author_kind = raw;
        assert_eq!(
            message(&test_support::module(), event).author().origin(),
            expected
        );
    }
    for (raw, expected) in [
        (MessageTargeting::DirectMessage, Targeting::DirectMessage),
        (MessageTargeting::Ambient, Targeting::Ambient),
        (
            MessageTargeting::GuildDirected(MentionKind::DirectMention),
            Targeting::DirectMention,
        ),
        (
            MessageTargeting::GuildDirected(MentionKind::ReplyToConstruct),
            Targeting::ReplyToConstruct,
        ),
        (
            MessageTargeting::GuildDirected(MentionKind::ConfiguredPattern),
            Targeting::ConfiguredPattern,
        ),
    ] {
        let mut event = fixture();
        event.targeting = raw;
        assert_eq!(
            message(&test_support::module(), event).targeting(),
            expected
        );
    }
    for (raw, expected) in [
        (bell_rings::BellStatus::Ok, BellStatus::Ok),
        (
            bell_rings::BellStatus::PartialTimeout,
            BellStatus::PartialTimeout,
        ),
        (
            bell_rings::BellStatus::PartialError,
            BellStatus::PartialError,
        ),
        (bell_rings::BellStatus::Timeout, BellStatus::Timeout),
        (bell_rings::BellStatus::Error, BellStatus::Error),
    ] {
        let mut event = fixture();
        event.bells_status = Some(raw);
        assert_eq!(
            message(&test_support::module(), event).bells().status(),
            Some(expected)
        );
    }
}

#[test]
fn ordered_evidence_keeps_legacy_spelling_and_original_author_text() {
    let mut event = fixture();
    event.content =
        "claim [🔍=v2:citation:AAAAAAAAAAw] [🔍=v2:claim:AAAAAAAAACI] [🔍=v1:AAAAAAAAAAw]".into();
    let converted = message(&test_support::module(), event);
    assert_eq!(
        converted
            .content()
            .locators()
            .iter()
            .map(|value| value.as_str())
            .collect::<Vec<_>>(),
        [
            "v2:citation:AAAAAAAAAAw",
            "v2:claim:AAAAAAAAACI",
            "v1:AAAAAAAAAAw"
        ]
    );
    assert_eq!(
        converted.content().text(),
        "claim [🔍=v2:citation:AAAAAAAAAAw] [🔍=v2:claim:AAAAAAAAACI] [🔍=v1:AAAAAAAAAAw]"
    );
    for content in [
        "redacted",
        "`example [🔍=v1:AAAAAAAAAAw]`",
        "claim [🔍=v1:AAAAAAAAAAw] afterwards",
    ] {
        let mut event = fixture();
        event.content = content.into();
        assert!(
            message(&test_support::module(), event)
                .content()
                .locators()
                .is_empty()
        );
    }
}

#[test]
fn managed_descriptor_uses_the_retained_message_and_cannot_override_it() {
    let module = test_support::module();
    for directive in [AttentionDirective::Prompt, AttentionDirective::NextTurn] {
        let delivery = Delivery::managed(
            message(&module, fixture()),
            directive,
            RecordId::new(" record ").unwrap(),
            test_support::context(),
        );
        let descriptor = delivery.attention().unwrap();
        assert_eq!(descriptor.record().as_str(), " record ");
        assert_eq!(descriptor.directive(), directive);
        assert_eq!(
            descriptor.source(),
            &source(&module, ChannelId::new(10), DiscordMessageId::new(20))
        );
        assert_eq!(descriptor.author(), &actor(&module, UserId::new(30)));
        assert_eq!(descriptor.original_text(), "exact original\n\n");
    }
}

#[test]
fn permission_is_a_control_and_mixed_batches_fail_before_any_projection() {
    let module = test_support::module();
    for granted in [true, false] {
        let delivery = single(
            &module,
            NotificationEvent::PermissionResponse {
                request_id: " token ".into(),
                granted,
            },
            test_support::context(),
        )
        .unwrap();
        let DeliveryKind::Control(ControlEvent::Permission { request, decision }) = delivery.kind()
        else {
            panic!("permission control");
        };
        assert_eq!(request.as_str(), " token ");
        assert_eq!(
            *decision,
            if granted {
                PermissionDecision::Granted
            } else {
                PermissionDecision::Denied
            }
        );
    }
    assert!(
        single(
            &module,
            NotificationEvent::PermissionResponse {
                request_id: String::new(),
                granted: true
            },
            test_support::context()
        )
        .is_err()
    );
    for controls_first in [true, false] {
        let permission = NotificationEvent::PermissionResponse {
            request_id: "valid".into(),
            granted: true,
        };
        let ordinary = NotificationEvent::Message(fixture());
        let (first, rest) = if controls_first {
            (permission, ordinary)
        } else {
            (ordinary, permission)
        };
        assert!(batch(&module, first, vec![rest], test_support::context()).is_err());
    }
    assert!(
        batch(
            &module,
            NotificationEvent::Message(fixture()),
            vec![NotificationEvent::Message(fixture())],
            test_support::context()
        )
        .is_ok()
    );
}

#[test]
fn coordination_preserves_validated_numeric_identities() {
    let event = NotificationEvent::Coordination(CoordinationEvent::Done {
        message_id: DiscordMessageId::new(1),
        channel_id: ChannelId::new(2),
        bot_id: UserId::new(3),
        reply_message_id: DiscordMessageId::new(4),
    });
    let ConvertedEvent::Control(ControlEvent::Coordination(notice)) =
        convert(&test_support::module(), event).unwrap()
    else {
        panic!("coordination control");
    };
    let CoordinationNotice::Done {
        claimed,
        responder,
        reply,
    } = notice.as_ref()
    else {
        panic!("done notice");
    };
    assert_eq!(claimed.local().as_numeric().unwrap().get(), 1);
    assert_eq!(
        claimed.conversation().local().as_numeric().unwrap().get(),
        2
    );
    assert_eq!(responder.local().as_numeric().unwrap().get(), 3);
    assert_eq!(reply.local().as_numeric().unwrap().get(), 4);
    assert_eq!(claimed.conversation(), reply.conversation());
}
