//! Historical compact-format expectations exercised through typed conversion.
use super::*;
use crate::{
    discord::events::{MessageEvent, NotificationEvent},
    harness::test_support,
    timestamp::Timestamp,
};
use chrono_tz::Tz;
use serde_json::Value;
use serenity::model::id::{ChannelId, MessageId, UserId};
#[derive(Debug)]
enum RenderedFixture {
    Single,
    Coalesced(Value),
}
fn render_fixture(events: Vec<NotificationEvent>, tz: Option<Tz>) -> Option<RenderedFixture> {
    let mut events = events.into_iter();
    let first = events.next()?;
    let rest: Vec<_> = events.collect();
    if rest.is_empty() {
        return Some(RenderedFixture::Single);
    }
    let delivery = crate::discord::delivery::batch(
        &test_support::module(),
        first,
        rest,
        PresentationContext::new(tz, EvidenceVisibility::Enabled),
    )
    .unwrap();
    Some(RenderedFixture::Coalesced(
        crate::codex::legacy_delivery::encode(
            &delivery,
            &crate::codex::LegacyDiscordScope::new(test_support::module()),
        )
        .unwrap(),
    ))
}
fn msg_event(channel: u64, msg_id: u64, user: &str, content: &str) -> NotificationEvent {
    NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(channel),
        message_id: MessageId::new(msg_id),
        user: user.to_string(),
        user_id: UserId::new(100),
        author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
        content: content.to_string(),
        targeting: crate::discord::events::MessageTargeting::Ambient,
        timestamp: Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: None,
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    })
}

fn reaction_event(channel: u64, msg_id: u64) -> NotificationEvent {
    NotificationEvent::Reaction {
        chat_id: ChannelId::new(channel),
        message_id: MessageId::new(msg_id),
        user: "bob".to_string(),
        user_id: UserId::new(200),
        emoji: "\u{1f44d}".to_string(),
        self_react: false,
    }
}

fn edit_event(channel: u64, msg_id: u64) -> NotificationEvent {
    NotificationEvent::MessageEdit {
        chat_id: ChannelId::new(channel),
        message_id: MessageId::new(msg_id),
        user: "alice".to_string(),
        user_id: UserId::new(100),
        new_content: "edited content".to_string(),
        timestamp: Timestamp::parse("2026-06-19T15:31:00+00:00").unwrap(),
        thread_parent_id: None,
        reply_to_message_id: None,
    }
}

fn delete_event(channel: u64, msg_id: u64) -> NotificationEvent {
    NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(channel),
        message_id: MessageId::new(msg_id),
        thread_parent_id: None,
    }
}

#[test]
fn create_edit_and_delete_have_transport_neutral_headers() {
    let create = msg_event(1, 2, "alice", "hello");
    let edit = edit_event(1, 2);
    let delete = delete_event(1, 2);
    let RenderedFixture::Coalesced(value) =
        render_fixture(vec![create, edit, delete], None).expect("coalesced")
    else {
        panic!("expected coalesced delivery");
    };
    let rendered = value["params"]["content"].as_str().expect("content");
    assert!(!rendered.contains("|A="));
    assert!(!rendered.contains("|@"));
    assert!(rendered.contains("!delete|2\n"));
}

#[test]
fn heterogeneous_events_preserve_each_visible_name_for_one_effective_user() {
    let create = msg_event(1, 2, "alice", "hello");
    let mut edit = edit_event(1, 2);
    let NotificationEvent::MessageEdit { user, .. } = &mut edit else {
        unreachable!()
    };
    *user = "bob".to_owned();
    let RenderedFixture::Coalesced(value) =
        render_fixture(vec![create, edit], None).expect("coalesced")
    else {
        panic!("expected coalesced delivery");
    };
    let rendered = value["params"]["content"].as_str().expect("content");
    assert!(rendered.contains("[users 100=alice 100=bob]"));
    assert!(rendered.contains("2|15:30|alice|L=1\nhello"));
    assert!(rendered.contains("!edit|2|15:31|bob|L=1\nedited content"));
}

#[test]
fn two_messages_same_channel_coalesced() {
    let events = vec![
        msg_event(1, 100, "alice", "hello"),
        msg_event(1, 101, "bob", "world"),
    ];
    let result = render_fixture(events, None);
    match result {
        Some(RenderedFixture::Coalesced(v)) => {
            assert_eq!(v["method"], "notifications/claude/channel");
            assert_eq!(v["params"]["meta"]["chat_id"], "1");
            assert_eq!(v["params"]["meta"]["format"], "batch_v1");
            assert_eq!(v["params"]["meta"]["type"], "batch");
            // Content should contain the batch format.
            let content = v["params"]["content"].as_str().unwrap();
            assert!(content.contains("[batch ch=1"));
            assert!(content.contains("alice"));
            assert!(content.contains("hello"));
            assert!(content.contains("world"));
        }
        other => panic!("expected Coalesced, got {other:?}"),
    }
}

#[test]
fn mixed_events_use_heterogeneous_format() {
    let events = vec![msg_event(1, 100, "alice", "hello"), reaction_event(1, 100)];
    let result = render_fixture(events, None);
    match result {
        Some(RenderedFixture::Coalesced(v)) => {
            assert_eq!(v["method"], "notifications/claude/channel");
            assert_eq!(v["params"]["meta"]["format"], "events_v1");
            assert_eq!(v["params"]["meta"]["type"], "batch");
            let content = v["params"]["content"].as_str().unwrap();
            assert!(content.contains("[events ch=1"));
            assert!(content.contains("hello"));
            assert!(content.contains("!react|100"));
        }
        other => panic!("expected Coalesced, got {other:?}"),
    }
}

#[test]
fn heterogeneous_with_edit_and_delete() {
    let events = vec![
        msg_event(1, 100, "alice", "original"),
        edit_event(1, 100),
        delete_event(1, 101),
    ];
    let result = render_fixture(events, None);
    match result {
        Some(RenderedFixture::Coalesced(v)) => {
            let content = v["params"]["content"].as_str().unwrap();
            assert!(content.contains("[events ch=1"));
            assert!(content.contains("n=3"));
            assert!(content.contains("!edit|100"));
            assert!(content.contains("edited content"));
            assert!(content.contains("!delete|101"));
        }
        other => panic!("expected Coalesced, got {other:?}"),
    }
}

#[test]
fn different_channels_produce_multi_channel_batch() {
    let events = vec![
        msg_event(1, 100, "alice", "in channel 1"),
        msg_event(2, 200, "bob", "in channel 2"),
    ];
    let result = render_fixture(events, None);
    match result {
        Some(RenderedFixture::Coalesced(v)) => {
            // Multiple channels packed into a single standard notification.
            assert_eq!(v["method"], "notifications/claude/channel");
            assert_eq!(v["params"]["meta"]["type"], "batch");
            assert_eq!(v["params"]["meta"]["format"], "multi_channel");
            // Content should contain both channels' batch text.
            let content = v["params"]["content"].as_str().unwrap();
            assert!(content.contains("ch=1"));
            assert!(content.contains("ch=2"));
            assert!(content.contains("in channel 1"));
            assert!(content.contains("in channel 2"));
        }
        other => panic!("expected Coalesced, got {other:?}"),
    }
}

#[test]
fn roster_includes_all_participants() {
    let events = vec![
        msg_event(1, 100, "alice", "hello"),
        reaction_event(1, 100),
        edit_event(1, 100),
    ];
    let result = render_fixture(events, None);
    match result {
        Some(RenderedFixture::Coalesced(v)) => {
            let content = v["params"]["content"].as_str().unwrap();
            // alice (user_id=100) and bob (user_id=200) should be in roster.
            assert!(content.contains("alice"));
            assert!(content.contains("bob"));
        }
        other => panic!("expected Coalesced, got {other:?}"),
    }
}

#[test]
fn timestamp_formatting() {
    let ts1 = Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap();
    assert_eq!(format_compact(&ts1, None), "15:30");
    let ts2 = Timestamp::parse("2026-06-19T15:30:45+00:00").unwrap();
    assert_eq!(format_compact(&ts2, None), "15:30:45");
}

// ── Wire-contract snapshots ─────────────────────────────────────────

#[test]
fn wire_format_snapshot_events_v1() {
    // Pin the events_v1 heterogeneous envelope format.
    // A message, a reaction, an edit, and a delete — the four event types
    // that appear in a mixed-event batch for a single channel.
    let tz: chrono_tz::Tz = "US/Eastern".parse().expect("valid tz");

    let events = vec![
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(555),
            message_id: MessageId::new(3001),
            user: "lina".to_string(),
            user_id: UserId::new(10),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
            content: "hey everyone".to_string(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-06-19T20:00:00+00:00").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: Some(ChannelId::new(666)),
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        }),
        NotificationEvent::Reaction {
            chat_id: ChannelId::new(555),
            message_id: MessageId::new(3001),
            user: "ros".to_string(),
            user_id: UserId::new(20),
            emoji: "\u{2764}\u{fe0f}".to_string(),
            self_react: false,
        },
        NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(555),
            message_id: MessageId::new(3001),
            user: "lina".to_string(),
            user_id: UserId::new(10),
            new_content: "hey everyone!".to_string(),
            timestamp: Timestamp::parse("2026-06-19T20:01:00+00:00").unwrap(),
            thread_parent_id: Some(ChannelId::new(666)),
            reply_to_message_id: None,
        },
        NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(555),
            message_id: MessageId::new(2999),
            thread_parent_id: Some(ChannelId::new(666)),
        },
    ];

    let result = render_fixture(events, Some(tz));
    let v = match result {
        Some(RenderedFixture::Coalesced(v)) => v,
        other => panic!("expected Coalesced, got {other:?}"),
    };

    assert_eq!(v["method"], "notifications/claude/channel");
    assert_eq!(v["params"]["meta"]["chat_id"], "555");
    assert_eq!(v["params"]["meta"]["format"], "events_v1");
    assert_eq!(v["params"]["meta"]["type"], "batch");

    let content = v["params"]["content"].as_str().unwrap();
    let expected = "\
[events ch=555 thread=666 n=4 tz=US/Eastern]
[users 10=lina 20=ros]

3001|16:00|lina|L=1
hey everyone

!react|3001|ros|\u{2764}\u{fe0f}

!edit|3001|16:01|lina|L=1
hey everyone!

!delete|2999
";
    assert_eq!(content, expected);
}
