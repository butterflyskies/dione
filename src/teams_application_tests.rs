use crate::{
    codex::{CodexEventQueue, CodexThreadId, LegacyDiscordScope},
    teams_test_process::{AdmissionResult, FixtureProcess, ReplyResult},
};
use dione_provider_delivery::ReplyHandle;
use dione_teams::{
    ReplyStoreError,
    edge::TeamsEdgeError,
    runtime::{TeamsRuntimeError, TeamsRuntimeErrorKind},
};
use serde_json::json;
use std::time::Duration;
use tempfile::TempDir;

const TENANT_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

fn discord_scope() -> LegacyDiscordScope {
    LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
        dione_contracts::ProviderId::new("discord").unwrap(),
        dione_contracts::ModuleSlotId::new("primary").unwrap(),
    ))
}

#[tokio::test]
async fn composed_edge_confines_reply_and_stops_before_token_on_failed_admission() {
    let queue_dir = TempDir::new().unwrap();
    let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned()).unwrap();
    let queue = CodexEventQueue::load(&queue_path, discord_scope()).unwrap();
    let resident_thread = CodexThreadId::parse("thread-janus").unwrap();
    let mut fixture = FixtureProcess::start(
        crate::composition::teams_admission::from_codex(queue.clone()),
        "activity/id",
    )
    .await;
    let activity = fixture.activity("activity/id");
    fixture.begin(1, &activity).await;
    assert!(matches!(
        fixture.wait(1).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(queue.status().await.queued, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(queue.status().await.queued, 0);
    assert_eq!(fixture.snapshot().await.pending_count, 0);

    queue
        .binding_controller()
        .bind_live_thread(Some(resident_thread.clone()))
        .await
        .unwrap();
    fixture.begin(2, &activity).await;
    assert!(matches!(
        fixture.wait(2).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(queue.status().await.queued, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(queue.status().await.queued, 0);
    let consumer = queue.register_live_consumer().await.unwrap();
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert!(
        consumer
            .next_event(
                &CodexThreadId::parse("thread-other").unwrap(),
                Duration::ZERO,
                Duration::from_secs(30)
            )
            .await
            .unwrap()
            .is_none()
    );
    let delivered = consumer
        .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(delivered.event["params"]["content"], "hello");
    assert_eq!(delivered.event["params"]["meta"]["provider"], "teams");
    assert_eq!(delivered.event["params"]["meta"]["tenant_id"], TENANT_ID);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert!(
        consumer
            .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .unwrap()
            .is_none()
    );
    let reply_handle = delivered.event["params"]["meta"]["reply_handle"]
        .as_str()
        .unwrap();
    assert!(reply_handle.starts_with("teams-"));
    assert!(!reply_handle.contains("conversation/id"));
    let reply_handle = ReplyHandle::parse(reply_handle).unwrap();

    fixture.close().await;
    fixture
        .reopen(crate::composition::teams_admission::from_codex(
            queue.clone(),
        ))
        .await;
    let ReplyResult::Sent {
        incoming_activity_id,
        outgoing_activity_id,
        conversation_id,
    } = fixture.reply(&reply_handle, "resident reply").await
    else {
        panic!("restored leased native authority must reply")
    };
    assert_eq!(incoming_activity_id.as_str(), "activity/id");
    assert_eq!(outgoing_activity_id.as_str(), "outgoing-activity-id");
    assert_eq!(conversation_id.as_str(), "conversation/id");
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.token_requests, 1);
    assert_eq!(snapshot.replies.len(), 1);
    assert_eq!(
        snapshot.replies[0].0,
        "/v3/conversations/conversation%2Fid/activities/activity%2Fid"
    );
    assert_eq!(
        snapshot.replies[0].1,
        json!({
            "type":"message", "from":{"id":"bot-id"}, "recipient":{"id":"sender-id"},
            "conversation":{"id":"conversation/id"}, "replyToId":"activity/id", "text":"resident reply"
        })
    );
    assert!(matches!(
        fixture.reply(&reply_handle, "must not send twice").await,
        ReplyResult::UnknownReplyHandle
    ));
    consumer
        .acknowledge(&delivered.delivery_token)
        .await
        .unwrap();
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert_eq!(queue.status().await.queued, 0);
    assert_eq!(fixture.snapshot().await.replies.len(), 1);

    let second_activity = fixture.activity("other/activity");
    assert_eq!(
        fixture.ingress(&second_activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    let second = consumer
        .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    let second_handle = ReplyHandle::parse(
        second.event["params"]["meta"]["reply_handle"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        fixture
            .reply(&second_handle, "simulate uncertain outcome")
            .await,
        ReplyResult::HttpUncertain
    ));
    assert!(matches!(
        fixture.reply(&second_handle, "must not resend").await,
        ReplyResult::UnknownReplyHandle
    ));
    assert_eq!(fixture.snapshot().await.replies.len(), 2);

    let mut denied: serde_json::Value = serde_json::from_slice(&activity).unwrap();
    denied["channelData"]["tenant"]["id"] = json!("wrong-tenant");
    let denied = serde_json::to_vec(&denied).unwrap();
    fixture.begin(3, &denied).await;
    assert!(matches!(
        fixture.wait(3).await,
        AdmissionResult::TenantDenied
    ));
    assert_eq!(
        fixture.ingress(&denied).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.token_requests, 2);
    assert_eq!(snapshot.replies.len(), 2);
    let queued_before = queue.status().await.queued;
    let mut excluded: serde_json::Value = serde_json::from_slice(&activity).unwrap();
    excluded["conversation"]["id"] = json!("excluded/native room");
    let excluded = serde_json::to_vec(&excluded).unwrap();
    fixture.begin(4, &excluded).await;
    assert!(matches!(
        fixture.wait(4).await,
        AdmissionResult::ConversationDenied
    ));
    assert_eq!(
        fixture.ingress(&excluded).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(queue.status().await.queued, queued_before);
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.token_requests, 2);
    assert_eq!(snapshot.replies.len(), 2);

    assert_legacy_consumer_denial_and_recovery().await;

    consumer.acknowledge(&second.delivery_token).await.unwrap();
    assert_eq!(
        fixture
            .ingress(&fixture.activity("invalid-receipt/activity"))
            .await,
        reqwest::StatusCode::ACCEPTED
    );
    let invalid_receipt_event = consumer
        .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    let invalid_receipt_handle = ReplyHandle::parse(
        invalid_receipt_event.event["params"]["meta"]["reply_handle"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        fixture
            .reply(&invalid_receipt_handle, "simulate malformed receipt")
            .await,
        ReplyResult::DecodeUncertain
    ));
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.replies.len(), 3);
    assert_eq!(snapshot.replies[2].1["text"], "simulate malformed receipt");
    assert_eq!(snapshot.token_requests, 3);
    assert!(matches!(
        fixture
            .reply(&invalid_receipt_handle, "must not retry malformed receipt")
            .await,
        ReplyResult::UnknownReplyHandle
    ));
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.replies.len(), 3);
    assert_eq!(snapshot.token_requests, 3);
    assert_eq!(snapshot.pending_count, 0);
    fixture.shutdown().await;
}

async fn assert_legacy_consumer_denial_and_recovery() {
    let legacy_dir = TempDir::new().unwrap();
    let legacy_path = camino::Utf8PathBuf::from_path_buf(legacy_dir.path().to_owned()).unwrap();
    let legacy_queue = CodexEventQueue::load(&legacy_path, discord_scope()).unwrap();
    let legacy_thread = CodexThreadId::parse("legacy-thread").unwrap();
    legacy_queue
        .binding_controller()
        .bind_live_thread(Some(legacy_thread.clone()))
        .await
        .unwrap();
    drop(legacy_queue);
    let inbox_path = legacy_path.join("codex-inbox.json");
    let mut disk: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&inbox_path).await.unwrap()).unwrap();
    disk["next_consumer_generation"] = json!(1);
    disk["primary_consumer"] = json!("codex-consumer-0");
    disk["consumers"] = json!([{
        "id":"codex-consumer-0", "label":"dione-live-app-server",
        "expires_at":chrono::Utc::now() + chrono::TimeDelta::hours(1), "ttl_seconds":3600
    }]);
    tokio::fs::write(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap())
        .await
        .unwrap();
    let legacy_queue = CodexEventQueue::load(&legacy_path, discord_scope()).unwrap();
    let mut fixture = FixtureProcess::start(
        crate::composition::teams_admission::from_codex(legacy_queue.clone()),
        "legacy/activity",
    )
    .await;
    let activity = fixture.activity("legacy/activity");
    fixture.begin(1, &activity).await;
    assert!(matches!(
        fixture.wait(1).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(legacy_queue.status().await.queued, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(legacy_queue.status().await.queued, 0);
    assert_eq!(fixture.snapshot().await.pending_count, 0);
    assert!(matches!(
        legacy_queue.register_live_consumer().await,
        Err(crate::codex::CodexQueueError {
            kind: crate::codex::CodexQueueErrorKind::Durable(dione_durable::QueueError {
                kind: dione_durable::QueueErrorKind::PrimaryConsumerExists,
                ..
            })
        })
    ));
    fixture.close().await;
    drop(legacy_queue);
    disk["consumers"][0]["expires_at"] = json!(chrono::Utc::now() - chrono::TimeDelta::seconds(1));
    tokio::fs::write(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap())
        .await
        .unwrap();
    let recovered_queue = CodexEventQueue::load(&legacy_path, discord_scope()).unwrap();
    let resident = recovered_queue.register_live_consumer().await.unwrap();
    fixture
        .reopen(crate::composition::teams_admission::from_codex(
            recovered_queue.clone(),
        ))
        .await;
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    let recovered_event = resident
        .next_event(&legacy_thread, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        recovered_event.event["params"]["meta"]["message_id"],
        "legacy/activity"
    );
    fixture.shutdown().await;
}

#[tokio::test]
async fn owned_admission_completion_retains_native_authority_after_queue_restart() {
    for lose_response in [false, true] {
        let directory = TempDir::new().unwrap();
        let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let scope = discord_scope();
        let queue = CodexEventQueue::open(&path, scope.clone()).await.unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let consumer_id = consumer.consumer_id().clone();
        let thread = CodexThreadId::parse("owned-admission").unwrap();
        consumer
            .bind_live_thread(Some(thread.clone()))
            .await
            .unwrap();
        let mut fixture = FixtureProcess::start(
            crate::composition::teams_admission::from_codex(queue.clone()),
            "owned/admission",
        )
        .await;
        let (reached, release) = queue.pause_owned_completion_for_test(lose_response);
        fixture.begin(1, &fixture.activity("owned/admission")).await;
        tokio::time::timeout(Duration::from_secs(2), reached)
            .await
            .unwrap()
            .unwrap();
        if lose_response {
            release.send(()).unwrap();
            assert!(matches!(
                fixture.wait(1).await,
                AdmissionResult::CompletionUnknown
            ));
            assert_eq!(fixture.join_panic_count(), 1);
        } else {
            fixture.cancel(1).await;
            release.send(()).unwrap();
        }
        queue.drain_owned().await;
        fixture.close().await;
        drop(consumer);
        drop(queue);
        let queue = CodexEventQueue::open(&path, scope).await.unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        assert_eq!(consumer.consumer_id(), &consumer_id);
        fixture
            .reopen(crate::composition::teams_admission::from_codex(
                queue.clone(),
            ))
            .await;
        let event = consumer
            .next_event(&thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            event.event["params"]["meta"]["message_id"],
            "owned/admission"
        );
        let handle = ReplyHandle::parse(
            event.event["params"]["meta"]["reply_handle"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let before = fixture.snapshot().await;
        assert!(matches!(
            fixture
                .reply(&handle, "reply after owned admission restart")
                .await,
            ReplyResult::Sent { .. }
        ));
        let after = fixture.snapshot().await;
        assert_eq!(after.replies.len(), before.replies.len() + 1);
        assert_eq!(after.token_requests, before.token_requests + 1);
        assert!(matches!(
            fixture.reply(&handle, "must not send twice").await,
            ReplyResult::UnknownReplyHandle
        ));
        let repeated = fixture.snapshot().await;
        assert_eq!(repeated.replies.len(), before.replies.len() + 1);
        assert_eq!(repeated.token_requests, before.token_requests + 1);
        fixture.shutdown().await;
    }
}

#[test]
fn teams_error_display_fragments_start_lowercase() {
    assert_eq!(
        TeamsRuntimeError {
            kind: TeamsRuntimeErrorKind::ReplyRegistryFull
        }
        .to_string(),
        "teams reply registry is full"
    );
    assert_eq!(
        ReplyStoreError {
            kind: dione_teams::ReplyStoreErrorKind::Full
        }
        .to_string(),
        "teams reply registry is full"
    );
    assert_eq!(
        TeamsEdgeError {
            kind: dione_teams::TeamsEdgeErrorKind::MetadataTooLarge
        }
        .to_string(),
        "teams metadata response exceeds its size limit"
    );
    assert_eq!(
        crate::codex::LiveQueueError {
            kind: crate::codex::LiveQueueErrorKind::ReplayCapacity(crate::codex::QueueError::new(
                "resident admission",
                crate::codex::QueueErrorKind::ReplayCapacity
            ))
        }
        .to_string(),
        "teams replay-identity capacity is full"
    );
}
