use dione_bell_rings::BellStatus;
use dione_discord_events::{MessageEvent, MessageTargeting, NotificationEvent};
use serde_json::json;
use serenity::model::id::{ChannelId, MessageId, UserId};
use {crate::harness::test_support::FixtureNotification, dione_messages::timestamp::Timestamp};

#[tokio::test]
async fn test_message_edit_includes_thread_parent_id() {
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["thread_parent_id"], "400");
    assert_eq!(meta["type"], "message_edit");
}

#[tokio::test]
async fn test_message_edit_omits_thread_parent_id_when_none() {
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("thread_parent_id").is_none());
}

#[tokio::test]
async fn message_edit_projects_legacy_evidence_as_citations_and_removal_clears_it() {
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

    let added = edit("edited [🔍=v1:AAAAAAAAAAw]")
        .fixture_notification()
        .await;
    assert_eq!(
        added["params"]["meta"]["citation_locators"],
        json!([{
            "locator": "v1:AAAAAAAAAAw",
            "author_id": "300",
        }])
    );
    assert!(added["params"]["meta"].get("claim_locators").is_none());

    let removed = edit("edited").fixture_notification().await;
    assert!(removed["params"]["meta"].get("claim_locators").is_none());
    assert!(removed["params"]["meta"].get("citation_locators").is_none());
}

#[tokio::test]
async fn disabled_evidence_projection_preserves_visible_content_only() {
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

    let json = event.fixture_notification_with_evidence(false).await;
    assert_eq!(json["params"]["content"], "edited [🔍=v1:AAAAAAAAAAw]");
    assert!(json["params"]["meta"].get("evidence").is_none());
    assert!(json["params"]["meta"].get("claim_locators").is_none());
    assert!(json["params"]["meta"].get("citation_locators").is_none());
}

#[tokio::test]
async fn test_message_delete_includes_thread_parent_id() {
    let event = NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        thread_parent_id: Some(ChannelId::new(500)),
    };
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["thread_parent_id"], "500");
    assert_eq!(meta["type"], "message_delete");
}

#[tokio::test]
async fn test_message_delete_omits_thread_parent_id_when_none() {
    let event = NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        thread_parent_id: None,
    };
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("thread_parent_id").is_none());
}

#[tokio::test]
async fn test_message_omits_thread_parent_id_when_none() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "bob".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "hello from channel".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("thread_parent_id").is_none());
}

#[tokio::test]
async fn test_message_includes_thread_parent_id() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "bob".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "hello from thread".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["thread_parent_id"], "600");
}

// ── Reply-to-message-id tests ─────────────────────────────────────────────

#[tokio::test]
async fn test_message_includes_reply_to_message_id() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "replying to you".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["reply_to_message_id"], "999");
}

#[tokio::test]
async fn test_message_omits_reply_to_message_id_when_none() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "not a reply".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("reply_to_message_id").is_none());
}

#[tokio::test]
async fn test_message_includes_reply_context() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "replying to you".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["reply_to_user_id"], "888");
    assert_eq!(meta["reply_to_user"], "bob");
    assert_eq!(meta["reply_to_content_preview"], "original message");
}

#[tokio::test]
async fn test_message_omits_reply_context_when_none() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "not a reply".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("reply_to_user_id").is_none());
    assert!(meta.get("reply_to_user").is_none());
    assert!(meta.get("reply_to_content_preview").is_none());
}

#[tokio::test]
async fn test_message_omits_reply_context_but_keeps_reply_to_message_id() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "reply without hydrated parent".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["reply_to_message_id"], "999");
    assert!(meta.get("reply_to_user_id").is_none());
    assert!(meta.get("reply_to_user").is_none());
    assert!(meta.get("reply_to_content_preview").is_none());
}

#[tokio::test]
async fn test_message_includes_bells_when_present() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "hello with bells".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(
        meta["bells"],
        "3s lain/person-pace;2b lain/feedback-no-platitudes"
    );
    assert_eq!(meta["bells_status"], "ok");
}

#[tokio::test]
async fn test_message_omits_bells_when_none() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "hello without bells".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("bells").is_none());
}

#[tokio::test]
async fn test_message_omits_preview_but_keeps_author() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(300),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
        content: "reply to attachment-only parent".into(),
        targeting: dione_discord_events::MessageTargeting::Ambient,
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["reply_to_user_id"], "888");
    assert_eq!(meta["reply_to_user"], "bob");
    assert!(meta.get("reply_to_content_preview").is_none());
}

#[tokio::test]
async fn test_message_edit_includes_reply_to_message_id() {
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["reply_to_message_id"], "888");
}

#[tokio::test]
async fn test_message_edit_omits_reply_to_message_id_when_none() {
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
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert!(meta.get("reply_to_message_id").is_none());
}

#[tokio::test]
async fn message_envelope_has_no_application_transport_tells() {
    let event = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(100),
        message_id: MessageId::new(200),
        user: "alice".into(),
        user_id: UserId::new(4242),
        author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
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
    let notification = event.fixture_notification().await;
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

#[tokio::test]
async fn coordination_done_event_serializes_with_metadata() {
    let event = NotificationEvent::Coordination(dione_coordination::CoordinationEvent::Done {
        message_id: "7".parse().unwrap(),
        channel_id: "42".parse().unwrap(),
        bot_id: "111".parse().unwrap(),
        reply_message_id: "99".parse().unwrap(),
    });
    let json = event.fixture_notification().await;
    assert_eq!(json["method"], "notifications/claude/channel");
    let meta = &json["params"]["meta"];
    assert_eq!(meta["type"], "coordination");
    assert_eq!(meta["event"], "done");
    assert_eq!(meta["message_id"], "7");
    assert_eq!(meta["chat_id"], "42");
    assert_eq!(meta["by"], "111");
    assert_eq!(meta["reply_message_id"], "99");
}

#[tokio::test]
async fn coordination_promoted_event_serializes_without_reply_fields() {
    let event = NotificationEvent::Coordination(dione_coordination::CoordinationEvent::Promoted {
        message_id: "7".parse().unwrap(),
        channel_id: "42".parse().unwrap(),
    });
    let json = event.fixture_notification().await;
    let meta = &json["params"]["meta"];
    assert_eq!(meta["type"], "coordination");
    assert_eq!(meta["event"], "promoted");
    assert_eq!(meta["message_id"], "7");
    assert_eq!(meta["chat_id"], "42");
    assert!(meta.get("by").is_none());
    assert!(meta.get("reply_message_id").is_none());
}
