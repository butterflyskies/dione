//! Adapter behavior for the same typed live records and historical inbox codec.

use super::{ProtocolOutput, test_support};
use camino::Utf8PathBuf;
use dione_codex::{CodexEventQueue, LegacyDiscordScope};
use dione_contracts::{
    ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef, ModuleInstanceId,
};
use dione_messages::{
    delivery::Delivery,
    events::{
        AuthorOrigin, BellObservation, BellStatus, ByteSize, MessageCreated, ObservedAttachment,
        ObservedAuthor, OrdinaryEvent, ReplyObservation, Targeting,
    },
    timestamp::Timestamp,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::watch,
};

fn conversation(module: &ModuleInstanceId, id: u64) -> ConversationRef {
    ConversationRef::new(
        module.clone(),
        ConversationId::numeric(id.try_into().unwrap()),
    )
}
fn source(module: &ModuleInstanceId, channel: u64, id: u64) -> MessageRef {
    MessageRef::new(
        conversation(module, channel),
        MessageId::numeric(id.try_into().unwrap()),
    )
}
fn actor(module: &ModuleInstanceId, id: u64) -> ActorRef {
    ActorRef::new(module.clone(), ActorId::numeric(id.try_into().unwrap()))
}
fn message(module: &ModuleInstanceId, id: u64) -> MessageCreated {
    MessageCreated::new(
        source(module, 10, id),
        ObservedAuthor::new(
            actor(module, 30),
            "author".into(),
            AuthorOrigin::VerifiedWebhook,
        ),
        "claim [🔍=v1:AAAAAAAAAAw]".into(),
        Targeting::DirectMention,
        Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
    )
    .with_attachments(
        vec![ObservedAttachment::new(
            "voice.ogg".into(),
            Some("audio/ogg".into()),
            ByteSize::new(1025),
        )],
        true,
    )
    .with_context(
        Some(conversation(module, 9)),
        ReplyObservation::new(
            Some(source(module, 10, 19)),
            Some(actor(module, 29)),
            Some("parent".into()),
            Some("preview".into()),
        ),
    )
    .with_bells(BellObservation::new(
        Some("bell".into()),
        Some(BellStatus::PartialError),
    ))
}
fn expected_legacy() -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"claim [🔍=v1:AAAAAAAAAAw]","meta":{
        "chat_id":"10","message_id":"20","user_id":"30","user":"30=author","ts":"2026-01-01T00:00:00+00:00",
        "is_voice_message":true,"thread_parent_id":"9","reply_to_message_id":"19","reply_to_user_id":"29","reply_to_user":"parent","reply_to_content_preview":"preview",
        "attachment_count":"1","attachments":"voice.ogg (audio/ogg, 1KB)","bells":"bell","bells_status":"partial_error","citation_locators":[{"locator":"v1:AAAAAAAAAAw","author_id":"30"}]
    }}})
}
async fn read_line(reader: tokio::io::DuplexStream) -> Value {
    serde_json::from_str(
        &BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn actual_claude_and_omp_writers_project_the_same_typed_message() {
    let module = test_support::module();
    let delivery = Delivery::single(
        OrdinaryEvent::Message(Box::new(message(&module, 20))),
        test_support::context(),
    );
    let (writer, reader) = tokio::io::duplex(8192);
    let claude = dione_claude::connect(ProtocolOutput::new(writer), dione_codex::TOOL_NAMES);
    claude.deliver(&delivery).await.unwrap();
    let actual = read_line(reader).await;
    let mut expected = expected_legacy();
    let meta = expected["params"]["meta"].as_object_mut().unwrap();
    meta.insert("is_voice_message".into(), json!("true"));
    meta.insert(
        "citation_locators".into(),
        json!(r#"[{"author_id":"30","locator":"v1:AAAAAAAAAAw"}]"#),
    );
    assert_eq!(actual, expected);
    assert!(
        actual["params"]["meta"]
            .as_object()
            .unwrap()
            .values()
            .all(Value::is_string)
    );
    let (writer, reader) = tokio::io::duplex(8192);
    let omp = dione_omp::connect(
        ProtocolOutput::new(writer),
        None,
        crate::harness::TOOL_NAMES,
    );
    omp.deliver(&delivery).await.unwrap();
    let actual = read_line(reader).await;
    let mut expected = expected_legacy();
    expected["method"] = json!("notifications/dione/event");
    expected["params"]["version"] = json!(1);
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn typed_codex_delivery_reopens_with_literal_legacy_payload_and_create_dedup() {
    let directory = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let module = test_support::module();
    let scope = LegacyDiscordScope::new(module.clone());
    let queue = CodexEventQueue::load(&path, scope.clone()).unwrap();
    let (binding, _) = watch::channel(None);
    let connection = dione_codex::connect(queue, binding);
    let delivery = Delivery::single(
        OrdinaryEvent::Message(Box::new(message(&module, 20))),
        test_support::context(),
    );
    connection.deliver(&delivery).await.unwrap();
    connection.deliver(&delivery).await.unwrap();
    drop(connection);
    let queue = CodexEventQueue::load(&path, scope).unwrap();
    let consumer = queue
        .register_consumer("typed codec".into(), Duration::from_secs(60), true, true)
        .await
        .unwrap()
        .consumer_id;
    let leased = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.event, expected_legacy());
    queue
        .acknowledge(&consumer, &leased.delivery_token)
        .await
        .unwrap();
    assert!(
        queue
            .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn permission_control_uses_claude_permission_params_without_channel_metadata() {
    for (granted, behavior) in [(true, "allow"), (false, "deny")] {
        let (writer, reader) = tokio::io::duplex(2048);
        let connection =
            dione_claude::connect(ProtocolOutput::new(writer), dione_codex::TOOL_NAMES);
        let delivery = test_support::delivery(
            dione_discord_events::NotificationEvent::PermissionResponse {
                request_id: " request-token ".into(),
                granted,
            },
        );
        connection.deliver(&delivery).await.unwrap();
        assert_eq!(
            read_line(reader).await,
            json!({"jsonrpc":"2.0","method":"notifications/claude/channel/permission","params":{"request_id":" request-token ","behavior":behavior}})
        );
    }
}
