use dione_attention::types::SourceAuthorKind;
use dione_contracts::{ModuleInstanceId, ModuleSlotId, ProviderId};
use dione_discord_events::{
    AttachmentMeta, DeliveryConversionErrorKind, MentionKind, MessageEvent, MessageTargeting,
    NotificationEvent, batch, message, single,
};
use dione_messages::{
    delivery::{DeliveryKind, EvidenceVisibility, PresentationContext},
    events::{
        AuthorOrigin, ControlError, ControlErrorKind, ControlEvent, OrdinaryEvent,
        PermissionDecision, Targeting,
    },
    timestamp::Timestamp,
};
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::error::Error;

fn module() -> ModuleInstanceId {
    ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("external").unwrap(),
    )
}
fn reason(kind: MentionKind) -> u8 {
    match kind {
        MentionKind::DirectMention => 1,
        MentionKind::ReplyToConstruct => 2,
        MentionKind::ConfiguredPattern => 3,
    }
}
fn context() -> PresentationContext {
    PresentationContext::new(None, EvidenceVisibility::Enabled)
}
fn observed(text: &str) -> MessageEvent {
    MessageEvent {
        chat_id: ChannelId::new(10),
        message_id: MessageId::new(20),
        user: "author".into(),
        user_id: UserId::new(30),
        author_kind: SourceAuthorKind::DirectHuman,
        content: text.into(),
        targeting: MessageTargeting::GuildDirected(MentionKind::ReplyToConstruct),
        timestamp: Timestamp::parse("2026-01-01T01:02:03+02:00").unwrap(),
        attachments: vec![AttachmentMeta {
            name: "empty.txt".into(),
            content_type: None,
            size: 0,
        }],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    }
}

#[test]
fn public_conversion_preserves_scopes_and_order_without_authority() {
    let scope = module();
    let converted = message(&scope, observed("original\n"));
    assert_eq!(converted.source().conversation().module(), &scope);
    assert_eq!(
        converted
            .source()
            .conversation()
            .local()
            .as_numeric()
            .unwrap()
            .get(),
        10
    );
    assert_eq!(converted.source().local().as_numeric().unwrap().get(), 20);
    assert_eq!(
        converted
            .author()
            .actor()
            .local()
            .as_numeric()
            .unwrap()
            .get(),
        30
    );
    assert_eq!(converted.author().origin(), AuthorOrigin::DirectHuman);
    assert_eq!(converted.content().text(), "original\n");
    assert_eq!(converted.targeting(), Targeting::ReplyToConstruct);
    assert_eq!(converted.attachments()[0].size().get(), 0);
    let delivery = batch(
        &scope,
        NotificationEvent::Message(observed("first")),
        vec![NotificationEvent::Message(observed("second"))],
        context(),
    )
    .unwrap();
    let DeliveryKind::Ordinary {
        first: OrdinaryEvent::Message(first),
        rest,
    } = delivery.kind()
    else {
        panic!("ordered ordinary delivery");
    };
    assert_eq!(first.content().text(), "first");
    let [OrdinaryEvent::Message(second)] = rest.as_slice() else {
        panic!("one retained remaining message");
    };
    assert_eq!(second.content().text(), "second");
}

#[test]
fn public_controls_preserve_typed_causes_and_distinguish_mixed_batches() {
    let error = single(
        &module(),
        NotificationEvent::PermissionResponse {
            request_id: String::new(),
            granted: true,
        },
        context(),
    )
    .unwrap_err();
    let DeliveryConversionErrorKind::Control(source) = &error.kind else {
        panic!("typed control error");
    };
    assert!(matches!(source.kind, ControlErrorKind::EmptyPermission));
    assert!(
        error
            .source()
            .unwrap()
            .downcast_ref::<DeliveryConversionErrorKind>()
            .is_some()
    );
    let _concrete: &ControlError = source;
    assert_eq!(
        error.to_string(),
        "cannot convert Discord delivery: invalid control event: permission request must not be empty"
    );
    for granted in [true, false] {
        let value = single(
            &module(),
            NotificationEvent::PermissionResponse {
                request_id: " token ".into(),
                granted,
            },
            context(),
        )
        .unwrap();
        let DeliveryKind::Control(ControlEvent::Permission { request, decision }) = value.kind()
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
    let mixed = batch(
        &module(),
        NotificationEvent::Message(observed("ordinary")),
        vec![NotificationEvent::ConfigError {
            error: "rejected".into(),
        }],
        context(),
    )
    .unwrap_err();
    assert!(matches!(
        mixed.kind,
        DeliveryConversionErrorKind::MixedControl
    ));
    assert_eq!(
        mixed.to_string(),
        "cannot convert Discord delivery: a control event cannot be included in an ordinary batch"
    );
}

#[test]
fn evidence_recognition_is_exact_and_limited_to_message_text() {
    assert_eq!(
        [
            reason(MentionKind::DirectMention),
            reason(MentionKind::ReplyToConstruct),
            reason(MentionKind::ConfiguredPattern)
        ],
        [1, 2, 3]
    );
    for (text, offered) in [
        ("claim [🔍=v1:AAAAAAAAAAw]", true),
        ("`claim [🔍=v1:AAAAAAAAAAw]`", false),
        ("claim [🔍=v1:AAAAAAAAAAw] afterwards", false),
        ("plain", false),
    ] {
        assert_eq!(
            NotificationEvent::Message(observed(text)).has_offered_evidence(),
            offered
        );
        assert_eq!(
            NotificationEvent::MessageEdit {
                chat_id: ChannelId::new(10),
                message_id: MessageId::new(20),
                user: "author".into(),
                user_id: UserId::new(30),
                new_content: text.into(),
                timestamp: observed("").timestamp,
                thread_parent_id: None,
                reply_to_message_id: None
            }
            .has_offered_evidence(),
            offered
        );
    }
    assert!(
        !NotificationEvent::ConfigError {
            error: "claim [🔍=v1:AAAAAAAAAAw]".into()
        }
        .has_offered_evidence()
    );
    for (target, directed) in [
        (MessageTargeting::DirectMessage, true),
        (MessageTargeting::Ambient, false),
        (
            MessageTargeting::GuildDirected(MentionKind::DirectMention),
            true,
        ),
        (
            MessageTargeting::GuildDirected(MentionKind::ReplyToConstruct),
            true,
        ),
        (
            MessageTargeting::GuildDirected(MentionKind::ConfiguredPattern),
            true,
        ),
    ] {
        assert_eq!(target.is_directed(), directed);
    }
}
