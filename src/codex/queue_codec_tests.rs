//! Literal legacy restoration, exact identities and dual replay proof.
use super::*;
use crate::{
    codex::{CodexEventQueue, CodexQueueErrorKind},
    core::{ModuleInstanceId, ModuleSlotId, ProviderId},
    durable::{QueueErrorKind, SnapshotIssue},
};
use camino::Utf8PathBuf;
use serde_json::json;
use std::{error::Error, time::Duration};
use tempfile::TempDir;

fn scope() -> LegacyDiscordScope {
    LegacyDiscordScope::new(ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    ))
}
fn fixture() -> Value {
    json!({"next_id":1,"next_lease_generation":0,"next_consumer_generation":1,"primary_consumer":"codex-consumer-0","live_thread_id":null,"consumers":[{"id":"codex-consumer-0","label":"reader","expires_at":"2099-01-01T00:00:00Z"}],"entries":[{"id":0,"payload":{"unknown":{"retained":true}},"consumer_id":"codex-consumer-0","lease":{"token":"legacy token:alpha","consumer_id":"codex-consumer-0","expires_at":"2099-01-01T00:00:00Z"}}]})
}
fn write(directory: &TempDir, value: &Value) -> (Utf8PathBuf, Vec<u8>) {
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let bytes = serde_json::to_vec(value).unwrap();
    std::fs::write(root.join("codex-inbox.json"), &bytes).unwrap();
    // Keep literal wire fixtures private so their codec assertions remain reachable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("codex-inbox.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }
    (root, bytes)
}

#[test]
fn exact_stored_identities_reject_without_normalizing_or_rewriting() {
    let paths = [
        "/primary_consumer",
        "/consumers/0/id",
        "/entries/0/consumer_id",
        "/entries/0/lease/consumer_id",
    ];
    for path in paths {
        for invalid in ["", " consumer-a ", "bad/consumer", &"a".repeat(129)] {
            let directory = TempDir::new().unwrap();
            let mut value = fixture();
            *value.pointer_mut(path).unwrap() = json!(invalid);
            let (root, original) = write(&directory, &value);
            let error = match CodexEventQueue::load(&root, scope()) {
                Ok(_) => panic!("stored identity must reject"),
                Err(error) => error,
            };
            assert!(matches!(
                error.kind,
                CodexQueueErrorKind::Durable(crate::durable::QueueError {
                    kind: QueueErrorKind::Codec(_),
                    ..
                })
            ));
            assert_eq!(
                std::fs::read(root.join("codex-inbox.json")).unwrap(),
                original
            );
        }
    }
    for invalid in ["", " token", "token ", &"a".repeat(129)] {
        let directory = TempDir::new().unwrap();
        let mut value = fixture();
        value["entries"][0]["lease"]["token"] = json!(invalid);
        let (root, original) = write(&directory, &value);
        assert!(CodexEventQueue::load(&root, scope()).is_err());
        assert_eq!(
            std::fs::read(root.join("codex-inbox.json")).unwrap(),
            original
        );
    }
    let mut value = fixture();
    value["entries"][0]["live_thread_id"] = json!(" thread ");
    assert!(
        LegacyQueueCodec::new(scope())
            .decode(&serde_json::to_vec(&value).unwrap())
            .is_err()
    );
}

#[tokio::test]
async fn opaque_and_unicode_stored_tokens_acknowledge_through_public_parser() {
    for token in ["legacy token:alpha", "保留 token:α"] {
        let directory = TempDir::new().unwrap();
        let mut value = fixture();
        value["entries"][0]["lease"]["token"] = json!(token);
        let (root, _) = write(&directory, &value);
        let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
        queue
            .acknowledge(
                &ConsumerId::parse(" codex-consumer-0 ").unwrap(),
                &DeliveryToken::parse(token).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(queue.status().await.queued, 0);
        drop(queue);
        let reopened = CodexEventQueue::open(&root, scope()).await.unwrap();
        assert_eq!(reopened.status().await.queued, 0);
    }
}

#[test]
fn absent_counters_reconstruct_canonical_generated_identities_but_explicit_conflicts_reject() {
    let codec = LegacyQueueCodec::new(scope());
    let mut value = fixture();
    value
        .as_object_mut()
        .unwrap()
        .remove("next_consumer_generation");
    value
        .as_object_mut()
        .unwrap()
        .remove("next_lease_generation");
    value["entries"][0]["lease"]["token"] = json!("dione-0-7");
    let snapshot = codec.decode(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(snapshot.next_consumer_generation, 1);
    assert_eq!(snapshot.next_lease_generation, 8);
    value["entries"][0]["lease"]["token"] = json!("dione-00-7");
    assert_eq!(
        codec
            .decode(&serde_json::to_vec(&value).unwrap())
            .unwrap()
            .next_lease_generation,
        0
    );
    for field in ["next_consumer_generation", "next_lease_generation"] {
        value[field] = Value::Null;
        assert!(codec.decode(&serde_json::to_vec(&value).unwrap()).is_err());
        value.as_object_mut().unwrap().remove(field);
    }
    value["next_consumer_generation"] = json!(0);
    let directory = TempDir::new().unwrap();
    let (root, bytes) = write(&directory, &value);
    let error = match CodexEventQueue::load(&root, scope()) {
        Err(error) => error,
        Ok(_) => panic!("explicit stale counter must reject"),
    };
    assert!(matches!(
        error.kind,
        CodexQueueErrorKind::Durable(crate::durable::QueueError {
            kind: QueueErrorKind::Integrity(SnapshotIssue::StaleCounter),
            ..
        })
    ));
    assert_eq!(std::fs::read(root.join("codex-inbox.json")).unwrap(), bytes);
}

#[test]
fn version_and_trailing_rejections_preserve_concrete_decode_causes() {
    let codec = LegacyQueueCodec::new(scope());
    for version in [json!(1), json!("0"), Value::Null] {
        let mut value = fixture();
        value["version"] = version;
        assert!(matches!(
            codec
                .decode(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .kind,
            CodecErrorKind::UnsupportedVersion
        ));
    }
    let mut bytes = serde_json::to_vec(&fixture()).unwrap();
    bytes.extend_from_slice(b" trailing");
    let error = codec.decode(&bytes).unwrap_err();
    let CodecErrorKind::Decode(source) = error.kind else {
        panic!("decode cause")
    };
    let error = source.downcast_ref::<LegacyCodecError>().unwrap();
    assert!(matches!(error.kind, LegacyCodecErrorKind::Trailing(_)));
    assert!(
        error
            .source()
            .unwrap()
            .source()
            .unwrap()
            .is::<serde_json::Error>()
    );
}

#[tokio::test]
async fn dual_legacy_replay_keys_acknowledge_both_families_across_restart() {
    let directory = TempDir::new().unwrap();
    let mut value = fixture();
    value["consumers"][0]["kind"] = json!("live");
    value["consumers"][0]["label"] = json!("dione-live-app-server");
    value["live_thread_id"] = json!("thread-a");
    value["entries"][0]["live_thread_id"] = json!("thread-a");
    value["entries"][0]["lease"] = Value::Null;
    value["entries"][0]["discord_message_id"] = json!("42");
    let key = ProviderEventKey::teams(
        "tenant".parse().unwrap(),
        "bot".parse().unwrap(),
        "conversation".parse().unwrap(),
        "activity".parse().unwrap(),
    );
    value["entries"][0]["provider_event_key"] = serde_json::to_value(&key).unwrap();
    let (root, _) = write(&directory, &value);
    let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    let event = resident
        .next_event(
            &CodexThreadId::parse("thread-a").unwrap(),
            Duration::ZERO,
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
    resident.acknowledge(&event.delivery_token).await.unwrap();
    drop(resident);
    drop(queue);
    let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    assert!(
        !queue
            .enqueue(json!({"params":{"meta":{"message_id":"42"}}}))
            .await
            .unwrap()
    );
    assert!(matches!(
        queue
            .enqueue_live(json!({"literal":"replayed"}), key)
            .await
            .unwrap(),
        crate::codex::LiveAdmissionOutcome::Duplicate
    ));
    let snapshot = queue.test_snapshot().await;
    assert_eq!(
        snapshot
            .acknowledged
            .iter()
            .filter(|entry| matches!(entry.key, LegacyReplayIdentity::DiscordCreate(_)))
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .acknowledged
            .iter()
            .filter(|entry| matches!(entry.key, LegacyReplayIdentity::Provider(_)))
            .count(),
        1
    );
    assert_eq!(queue.status().await.queued, 0);
    drop(resident);
}

#[test]
fn literal_teams_snapshot_retains_wire_fields_and_payload_extensions() {
    let bytes = include_bytes!("../../tests/fixtures/teams/codex-inbox-scoped-identities.json");
    let codec = LegacyQueueCodec::new(scope());
    let snapshot = codec.decode(bytes).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&codec.encode(&snapshot).unwrap()).unwrap(),
        serde_json::from_slice::<Value>(bytes).unwrap()
    );
    assert_eq!(
        snapshot.entries[0].visibility,
        state::EntryVisibility::ResidentOnly
    );
}

#[test]
fn two_entries_cannot_share_a_restored_replay_identity() {
    let directory = TempDir::new().unwrap();
    let mut value = fixture();
    value["next_id"] = json!(2);
    value["entries"][0]["discord_message_id"] = json!("42");
    value["entries"][0]["lease"] = Value::Null;
    let mut second = value["entries"][0].clone();
    second["id"] = json!(1);
    value["entries"].as_array_mut().unwrap().push(second);
    let (root, original) = write(&directory, &value);
    let error = match CodexEventQueue::load(&root, scope()) {
        Err(error) => error,
        Ok(_) => panic!("duplicate queued replay identity must reject"),
    };
    assert!(matches!(
        error.kind,
        CodexQueueErrorKind::Durable(crate::durable::QueueError {
            kind: QueueErrorKind::Integrity(SnapshotIssue::DuplicateReplay),
            ..
        })
    ));
    assert_eq!(
        std::fs::read(root.join("codex-inbox.json")).unwrap(),
        original
    );
}

#[tokio::test]
async fn dual_replay_invalidation_rolls_back_then_removes_both_indices() {
    let directory = TempDir::new().unwrap();
    let mut value = fixture();
    let key = ProviderEventKey::teams(
        "tenant".parse().unwrap(),
        "bot".parse().unwrap(),
        "conversation".parse().unwrap(),
        "activity".parse().unwrap(),
    );
    value["entries"][0]["discord_message_id"] = json!("42");
    value["entries"][0]["provider_event_key"] = serde_json::to_value(&key).unwrap();
    value["entries"][0]["payload"] = json!({"method":"notifications/claude/channel","params":{"content":"managed","meta":{"message_id":"42","chat_id":"43","user_id":"44","attention_delivery":"prompt","attention_record":"record-1"}}});
    let (root, original) = write(&directory, &value);
    let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
    let record = crate::attention::types::RecordId::new("record-1").unwrap();
    let codec = LegacyQueueCodec::new(scope());
    let before = codec.encode(&queue.test_snapshot().await).unwrap();
    queue
        .test_set_persist_failure(Some(crate::durable::PersistFailure::BeforeRename))
        .await;
    assert!(
        queue
            .invalidate_attention(crate::codex::AttentionInvalidation::Record(&record))
            .await
            .is_err()
    );
    assert_eq!(codec.encode(&queue.test_snapshot().await).unwrap(), before);
    assert_eq!(
        std::fs::read(root.join("codex-inbox.json")).unwrap(),
        original
    );
    queue.test_set_persist_failure(None).await;
    assert_eq!(
        queue
            .invalidate_attention(crate::codex::AttentionInvalidation::Record(&record))
            .await
            .unwrap(),
        crate::codex::AttentionInvalidationResult {
            removed: 1,
            invalidated_leases: 1
        }
    );
    assert!(
        queue
            .enqueue(json!({"params":{"meta":{"message_id":"42"}}}))
            .await
            .unwrap()
    );
    let snapshot = queue.test_snapshot().await;
    assert_eq!(snapshot.entries.len(), 1);
    assert!(
        snapshot
            .entries
            .iter()
            .flat_map(|entry| &entry.replay_keys)
            .all(|key| matches!(key, LegacyReplayIdentity::DiscordCreate(_)))
    );
}

#[tokio::test]
async fn prior_durability_barrier_precedes_malformed_payload_validation() {
    let directory = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
    queue
        .test_set_persist_failure(Some(crate::durable::PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue
            .enqueue(json!({"literal":"retained"}))
            .await
            .unwrap_err()
            .kind,
        CodexQueueErrorKind::Durable(crate::durable::QueueError {
            kind: QueueErrorKind::DurabilityUncertain(_),
            ..
        })
    ));
    let malformed = json!({"params":{"meta":{"attention_delivery":"broken"}}});
    assert!(matches!(
        queue.enqueue(malformed.clone()).await.unwrap_err().kind,
        CodexQueueErrorKind::Durable(crate::durable::QueueError {
            kind: QueueErrorKind::PriorSync(_),
            ..
        })
    ));
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        queue.enqueue(malformed).await.unwrap_err().kind,
        CodexQueueErrorKind::LegacyAttention(_)
    ));
    assert_eq!(queue.status().await.queued, 1);
}

#[tokio::test]
async fn retained_resident_replay_precedes_malformed_payload_validation() {
    let directory = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let queue = CodexEventQueue::open(&root, scope()).await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    resident
        .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
        .await
        .unwrap();
    let key = ProviderEventKey::teams(
        "tenant".parse().unwrap(),
        "bot".parse().unwrap(),
        "conversation".parse().unwrap(),
        "activity".parse().unwrap(),
    );
    assert!(matches!(
        queue
            .enqueue_live(json!({"literal":"retained"}), key.clone())
            .await
            .unwrap(),
        crate::codex::LiveAdmissionOutcome::Committed
    ));
    assert!(matches!(
        queue
            .enqueue_live(
                json!({"params":{"meta":{"attention_delivery":"broken"}}}),
                key
            )
            .await
            .unwrap(),
        crate::codex::LiveAdmissionOutcome::Duplicate
    ));
    assert_eq!(queue.status().await.queued, 1);
}

#[test]
fn unrepresentable_snapshot_is_an_encode_failure_with_original_cause() {
    let codec = LegacyQueueCodec::new(scope());
    let bytes = serde_json::to_vec(&fixture()).unwrap();
    let mut snapshot = codec.decode(&bytes).unwrap();
    snapshot.endpoint = Some(EndpointId::new("foreign/endpoint").unwrap());
    let error = codec.encode(&snapshot).unwrap_err();
    let CodecErrorKind::Encode(source) = error.kind else {
        panic!("encoding must retain its actual phase")
    };
    assert!(matches!(
        source.downcast_ref::<LegacyCodecError>().unwrap().kind,
        LegacyCodecErrorKind::Payload(_)
    ));
    let mut snapshot = codec.decode(&bytes).unwrap();
    let key = LegacyReplayIdentity::from_payload(&json!({"params":{"meta":{"message_id":"42"}}}))
        .unwrap();
    snapshot.entries[0].replay_keys = vec![key.clone(), key];
    let error = codec.encode(&snapshot).unwrap_err();
    let CodecErrorKind::Encode(source) = error.kind else {
        panic!("encoding must retain its actual phase")
    };
    assert!(matches!(
        source.downcast_ref::<LegacyCodecError>().unwrap().kind,
        LegacyCodecErrorKind::ReplayShape
    ));
}
