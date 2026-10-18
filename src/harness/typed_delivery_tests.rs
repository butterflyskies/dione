//! Adapter behavior for the same typed live records and historical inbox codec.

use super::{HarnessConnection, ProtocolOutput, test_support};
use crate::{
    attention::{delivery::AttentionDirective, types::RecordId},
    codex::{CodexEventQueue, LegacyDiscordScope, legacy_delivery},
    core::{
        ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef,
        ModuleInstanceId, ModuleSlotId, ProviderId,
        delivery::{Delivery, DeliveryKind, EvidenceVisibility, PresentationContext},
        events::{
            AuthorOrigin, BellObservation, BellStatus, ByteSize, MessageCreated,
            ObservedAttachment, ObservedAuthor, OrdinaryEvent, ReplyObservation, Targeting,
        },
    },
    timestamp::Timestamp,
};
use camino::Utf8PathBuf;
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
    let claude = HarnessConnection::claude_with_output(ProtocolOutput::new(writer));
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
    let omp = HarnessConnection::oh_my_pi_with_output(ProtocolOutput::new(writer));
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
    let connection = HarnessConnection::codex(queue, binding);
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

#[test]
fn private_legacy_codec_rejects_every_foreign_or_opaque_nested_identity() {
    let module = test_support::module();
    let foreign = ModuleInstanceId::new(
        ProviderId::new("other").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    );
    let scope = LegacyDiscordScope::new(module.clone());
    let encode = |message| {
        legacy_delivery::encode(
            &Delivery::single(
                OrdinaryEvent::Message(Box::new(message)),
                test_support::context(),
            ),
            &scope,
        )
    };
    assert_eq!(encode(message(&module, 20)).unwrap(), expected_legacy());
    assert!(encode(message(&foreign, 20)).is_err());
    assert!(
        encode(
            message(&module, 20)
                .with_context(Some(conversation(&foreign, 9)), ReplyObservation::default())
        )
        .is_err()
    );
    assert!(
        encode(message(&module, 20).with_context(
            None,
            ReplyObservation::new(Some(source(&foreign, 10, 19)), None, None, None)
        ))
        .is_err()
    );
    assert!(
        encode(message(&module, 20).with_context(
            None,
            ReplyObservation::new(Some(source(&module, 11, 19)), None, None, None)
        ))
        .is_err()
    );
    assert!(
        encode(message(&module, 20).with_context(
            None,
            ReplyObservation::new(None, Some(actor(&foreign, 29)), None, None)
        ))
        .is_err()
    );
    let opaque = ActorRef::new(module.clone(), ActorId::opaque("29").unwrap());
    assert!(
        encode(
            message(&module, 20)
                .with_context(None, ReplyObservation::new(None, Some(opaque), None, None))
        )
        .is_err()
    );
    let opaque = MessageRef::new(conversation(&module, 10), MessageId::opaque("19").unwrap());
    assert!(
        encode(
            message(&module, 20)
                .with_context(None, ReplyObservation::new(Some(opaque), None, None, None))
        )
        .is_err()
    );
    let opaque = ConversationRef::new(module.clone(), ConversationId::opaque("9").unwrap());
    assert!(
        encode(message(&module, 20).with_context(Some(opaque), ReplyObservation::default()))
            .is_err()
    );
}

#[test]
fn both_managed_directives_decode_to_the_same_retained_authority_descriptor() {
    let module = test_support::module();
    let scope = LegacyDiscordScope::new(module.clone());
    for (directive, spelling) in [
        (AttentionDirective::Prompt, "prompt"),
        (AttentionDirective::NextTurn, "next_turn"),
    ] {
        let delivery = Delivery::managed(
            message(&module, 20),
            directive,
            RecordId::new("literal-record").unwrap(),
            test_support::context(),
        );
        let payload = legacy_delivery::encode(&delivery, &scope).unwrap();
        let mut expected = expected_legacy();
        expected["params"]["meta"]["attention_delivery"] = json!(spelling);
        expected["params"]["meta"]["attention_record"] = json!("literal-record");
        assert_eq!(payload, expected);
        let decoded = legacy_delivery::decode_attention(&payload, &scope)
            .unwrap()
            .unwrap();
        let descriptor = decoded.descriptor();
        let original = delivery.attention().unwrap();
        assert_eq!(descriptor.source(), original.source());
        assert_eq!(descriptor.author(), original.author());
        assert_eq!(descriptor.original_text(), "claim [🔍=v1:AAAAAAAAAAw]");
        assert_eq!(descriptor.directive(), directive);
        assert_eq!(descriptor.record().as_str(), "literal-record");
    }
}

#[test]
fn opaque_provider_batches_group_by_complete_scope_without_mutating_records() {
    let module = |slot| {
        ModuleInstanceId::new(
            ProviderId::new("fake").unwrap(),
            ModuleSlotId::new(slot).unwrap(),
        )
    };
    let first = module("a");
    let second = module("b");
    let make = |module: &ModuleInstanceId, name: &str| {
        OrdinaryEvent::Message(Box::new(MessageCreated::new(
            MessageRef::new(
                ConversationRef::new(module.clone(), ConversationId::opaque("room/x").unwrap()),
                MessageId::opaque("message/x").unwrap(),
            ),
            ObservedAuthor::new(
                ActorRef::new(module.clone(), ActorId::opaque("actor/x").unwrap()),
                name.into(),
                AuthorOrigin::DirectHuman,
            ),
            "exact\n\n".into(),
            Targeting::Ambient,
            Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        )))
    };
    let delivery = Delivery::batch(
        make(&second, "second"),
        vec![make(&first, "first")],
        PresentationContext::new(None, EvidenceVisibility::Disabled),
    );
    let DeliveryKind::Ordinary { first: head, rest } = delivery.kind() else {
        panic!("ordinary batch");
    };
    assert_eq!(head.source().conversation().module(), &second);
    assert_eq!(rest[0].source().conversation().module(), &first);
    let super::presentation::Presentation::Channel(projected) =
        super::presentation::project(&delivery)
    else {
        panic!("channel");
    };
    assert_eq!(
        projected.content,
        "[batch ch=room/x n=1 latest=message/x]\n[users actor/x=first]\n\nmessage/x|00:00|first|L=1\nexact\n\n[batch ch=room/x n=1 latest=message/x]\n[users actor/x=second]\n\nmessage/x|00:00|second|L=1\nexact\n"
    );
    assert_eq!(
        serde_json::to_value(projected.meta).unwrap(),
        json!({"chat_id":"room/x","type":"batch","format":"multi_channel"})
    );
    let OrdinaryEvent::Message(message) = head else {
        panic!("message");
    };
    assert_eq!(message.content().text(), "exact\n\n");
    assert!(
        legacy_delivery::encode(&delivery, &LegacyDiscordScope::new(test_support::module()))
            .is_err()
    );
}

#[tokio::test]
async fn permission_control_uses_claude_permission_params_without_channel_metadata() {
    for (granted, behavior) in [(true, "allow"), (false, "deny")] {
        let (writer, reader) = tokio::io::duplex(2048);
        let connection = HarnessConnection::claude_with_output(ProtocolOutput::new(writer));
        let delivery = test_support::delivery(
            crate::discord::events::NotificationEvent::PermissionResponse {
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
