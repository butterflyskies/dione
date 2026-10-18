use super::*;
use crate::{
    discord::events::{MessageEvent, MessageTargeting, NotificationEvent},
    harness::test_support,
    timestamp::Timestamp,
};
use serenity::model::id::{ChannelId, MessageId, UserId};
use tokio::io::{AsyncBufReadExt, BufReader};

#[test]
fn typed_permission_is_rejected_without_parsing_a_foreign_envelope() {
    let permission = test_support::delivery(NotificationEvent::PermissionResponse {
        request_id: "request".into(),
        granted: true,
    });
    assert!(matches!(
        notification(&permission),
        Err(ProjectionError {
            kind: ProjectionErrorKind::Permission
        })
    ));
    assert!(notification(&test_support::text("allowed")).is_ok());
}

#[test]
fn projects_real_rendered_message_reaction_config_and_trace() {
    let message = NotificationEvent::Message(MessageEvent {
        chat_id: ChannelId::new(10),
        message_id: MessageId::new(20),
        user: "author".into(),
        user_id: UserId::new(30),
        author_kind: Default::default(),
        content: "hello".into(),
        targeting: MessageTargeting::DirectMessage,
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        attachments: vec![],
        is_voice_message: false,
        thread_parent_id: None,
        reply_to_message_id: Some(MessageId::new(19)),
        reply_to_user_id: None,
        reply_to_user: None,
        reply_to_content_preview: None,
        bells: None,
        bells_status: None,
    });
    let cases = [
        (
            message,
            json!({"version":1,"content":"hello","meta":{"chat_id":"10","message_id":"20","user":"30=author","user_id":"30","ts":"2026-01-01T00:00:00+00:00","reply_to_message_id":"19"}}),
        ),
        (
            NotificationEvent::Reaction {
                chat_id: ChannelId::new(10),
                message_id: MessageId::new(20),
                user: "author".into(),
                user_id: UserId::new(30),
                emoji: "ok".into(),
                self_react: false,
            },
            json!({"version":1,"content":"reacted with ok","meta":{"chat_id":"10","message_id":"20","user":"30=author","user_id":"30","type":"reaction","emoji":"ok"}}),
        ),
        (
            NotificationEvent::ConfigError {
                error: "invalid key".into(),
            },
            json!({"version":1,"content":"config.toml parse error — running on last valid config. Error: invalid key","meta":{"type":"config_error"}}),
        ),
        (
            NotificationEvent::Trace {
                level: tracing::Level::INFO,
                target: "test".into(),
                message: "ready".into(),
                fields: vec![],
            },
            json!({"version":1,"content":"ready","meta":{"type":"trace","level":"INFO","target":"test"}}),
        ),
    ];
    for (event, expected) in cases {
        let rendered = test_support::delivery(event);
        assert_eq!(
            notification(&rendered).unwrap(),
            json!({"jsonrpc":"2.0","method":"notifications/dione/event","params":expected})
        );
    }
}

#[tokio::test]
async fn concurrent_response_and_event_are_complete_lines_and_rejection_writes_nothing() {
    let (writer, reader) = tokio::io::duplex(32);
    let connection = HarnessConnection::oh_my_pi_with_output(ProtocolOutput::new(writer));
    let permission = test_support::delivery(NotificationEvent::PermissionResponse {
        request_id: "no".into(),
        granted: true,
    });
    assert!(connection.deliver(&permission).await.is_err());
    let response = json!({"jsonrpc":"2.0","id":7,"result":{"content":[]}});
    let event = test_support::text("hello");
    let read = async {
        let mut lines = BufReader::new(reader).lines();
        let mut values = Vec::new();
        for _ in 0..2 {
            values.push(
                serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap()).unwrap(),
            );
        }
        values
    };
    let (response_result, delivered, values) = tokio::join!(
        connection.output().write(&response),
        connection.deliver(&event),
        read
    );
    response_result.unwrap();
    delivered.unwrap();
    assert!(values.contains(&json!({"jsonrpc":"2.0","id":7,"result":{"content":[]}})));
    assert!(values.contains(&json!({"jsonrpc":"2.0","method":"notifications/dione/event","params":{"version":1,"content":"hello","meta":{"type":"trace","level":"INFO","target":"fixture"}}})));
}

#[test]
fn projects_real_coalesced_metadata() {
    let events = [20, 21].map(|id| NotificationEvent::MessageDelete {
        chat_id: ChannelId::new(10),
        message_id: MessageId::new(id),
        thread_parent_id: None,
    });
    let [first, second] = events;
    let rendered = crate::discord::delivery::batch(
        &test_support::module(),
        first,
        vec![second],
        test_support::context(),
    )
    .unwrap();
    assert_eq!(
        notification(&rendered).unwrap(),
        json!({
            "jsonrpc":"2.0","method":"notifications/dione/event",
            "params":{"version":1,"content":"[events ch=10 n=2]\n\n!delete|20\n\n!delete|21\n","meta":{"chat_id":"10","type":"batch","format":"events_v1"}}
        })
    );
}

#[test]
fn projects_real_evidence_metadata() {
    let rendered = test_support::delivery(NotificationEvent::MessageEdit {
        chat_id: ChannelId::new(10),
        message_id: MessageId::new(20),
        user: "author".into(),
        user_id: UserId::new(30),
        new_content: "claim [🔍=v2:claim:AAAAAAAAAAw]".into(),
        timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        thread_parent_id: None,
        reply_to_message_id: None,
    });
    assert_eq!(
        notification(&rendered).unwrap(),
        json!({
            "jsonrpc":"2.0","method":"notifications/dione/event",
            "params":{"version":1,"content":"claim [🔍=v2:claim:AAAAAAAAAAw]","meta":{
                "chat_id":"10","message_id":"20","user":"30=author","user_id":"30","type":"message_edit","ts":"2026-01-01T00:00:00+00:00",
                "claim_locators":[{"locator":"v2:claim:AAAAAAAAAAw","author_id":"30"}]
            }}
        })
    );
}

#[tokio::test]
async fn identity_display_roundtrips_untrusted_names_without_changing_machine_ids() {
    let name = "Iris \"quoted\" \\ [group]\n(xe/it)";
    let events = [
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(10),
            message_id: MessageId::new(20),
            user: name.into(),
            user_id: UserId::new(30),
            author_kind: Default::default(),
            content: "hello".into(),
            targeting: MessageTargeting::DirectMessage,
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
        }),
        NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(10),
            message_id: MessageId::new(20),
            user: name.into(),
            user_id: UserId::new(30),
            new_content: "edited".into(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        },
        NotificationEvent::Reaction {
            chat_id: ChannelId::new(10),
            message_id: MessageId::new(20),
            user: name.into(),
            user_id: UserId::new(30),
            emoji: "ok".into(),
            self_react: false,
        },
    ];
    for event in events {
        let (writer, reader) = tokio::io::duplex(16 * 1024);
        let connection = HarnessConnection::oh_my_pi_with_output(ProtocolOutput::new(writer));
        connection
            .deliver(&test_support::delivery(event))
            .await
            .unwrap();
        let line = BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap();
        let delivered: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            delivered["params"]["meta"]["user"],
            "30=Iris \"quoted\" \\ [group]\n(xe/it)"
        );
        assert_eq!(delivered["params"]["meta"]["user_id"], "30");
        assert_eq!(delivered["params"]["meta"]["chat_id"], "10");
        assert_eq!(delivered["params"]["meta"]["message_id"], "20");
    }
}

#[test]
fn identity_display_batch_roster_keeps_actor_order_and_blank_fallback() {
    let event = |id, name: &str| NotificationEvent::Reaction {
        chat_id: ChannelId::new(10),
        message_id: MessageId::new(20),
        user: name.into(),
        user_id: UserId::new(id),
        emoji: "ok".into(),
        self_react: false,
    };
    let delivery = crate::discord::delivery::batch(
        &test_support::module(),
        event(20, "Later (she/her)"),
        vec![event(10, " ")],
        test_support::context(),
    )
    .unwrap();
    let wire = notification(&delivery).unwrap();
    let text = wire["params"]["content"].as_str().unwrap();
    assert!(
        text.starts_with("[events ch=10 n=2]\n[users 10 20=Later (she/her)]\n\n"),
        "{text}"
    );
    assert_eq!(wire["params"]["meta"]["chat_id"], "10");
    assert_eq!(wire["params"]["meta"]["format"], "events_v1");
}
