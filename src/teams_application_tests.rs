use crate::codex_test_process::OwnerProcess;
use crate::teams_test_process::{AdmissionResult, FixtureProcess, ReplyResult};
use dione_codex::CodexThreadId;
use dione_provider_delivery::ReplyHandle;
use dione_teams::{
    ReplyStoreError,
    edge::TeamsEdgeError,
    runtime::{TeamsRuntimeError, TeamsRuntimeErrorKind},
};
use serde_json::json;

const TENANT_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

#[tokio::test]
async fn composed_edge_confines_reply_and_stops_before_token_on_failed_admission() {
    let mut owner = OwnerProcess::start().await;
    let resident_thread = CodexThreadId::parse("thread-janus").unwrap();
    let mut fixture = FixtureProcess::start(owner.teams_delivery(), "activity/id").await;
    let activity = fixture.activity("activity/id");
    fixture.begin(1, &activity).await;
    assert!(matches!(
        fixture.wait(1).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(owner.queued().await, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(owner.queued().await, 0);
    assert_eq!(fixture.snapshot().await.pending_count, 0);

    let bound=owner.command(json!({"command":"call","name":"bind_codex_thread","args":{"thread_id":resident_thread.as_str()}})).await;
    assert!(
        bound["ok"].is_object(),
        "actual binding succeeds without creating a resident"
    );
    fixture.begin(2, &activity).await;
    assert!(matches!(
        fixture.wait(2).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(owner.queued().await, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(owner.queued().await, 0);
    owner.command(json!({"command":"resident"})).await;
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert!(
        owner
            .resident_next(&CodexThreadId::parse("thread-other").unwrap())
            .await
            .is_none()
    );
    let delivered = owner.resident_next(&resident_thread).await.unwrap();
    assert_eq!(delivered.event["params"]["content"], "hello");
    assert_eq!(delivered.event["params"]["meta"]["provider"], "teams");
    assert_eq!(delivered.event["params"]["meta"]["tenant_id"], TENANT_ID);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert!(owner.resident_next(&resident_thread).await.is_none());
    let reply_handle = delivered.event["params"]["meta"]["reply_handle"]
        .as_str()
        .unwrap();
    assert!(reply_handle.starts_with("teams-"));
    assert!(!reply_handle.contains("conversation/id"));
    let reply_handle = ReplyHandle::parse(reply_handle).unwrap();

    fixture.close().await;
    fixture.reopen(owner.teams_delivery()).await;
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
    owner.resident_ack(&delivered.delivery_token).await;
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    assert_eq!(owner.queued().await, 0);
    assert_eq!(fixture.snapshot().await.replies.len(), 1);

    let second_activity = fixture.activity("other/activity");
    assert_eq!(
        fixture.ingress(&second_activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    let second = owner.resident_next(&resident_thread).await.unwrap();
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
    let queued_before = owner.queued().await;
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
    assert_eq!(owner.queued().await, queued_before);
    let snapshot = fixture.snapshot().await;
    assert_eq!(snapshot.token_requests, 2);
    assert_eq!(snapshot.replies.len(), 2);

    assert_legacy_consumer_denial_and_recovery().await;

    owner.resident_ack(&second.delivery_token).await;
    assert_eq!(
        fixture
            .ingress(&fixture.activity("invalid-receipt/activity"))
            .await,
        reqwest::StatusCode::ACCEPTED
    );
    let invalid_receipt_event = owner.resident_next(&resident_thread).await.unwrap();
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
    owner.shutdown().await;
}

async fn assert_legacy_consumer_denial_and_recovery() {
    let mut owner = OwnerProcess::start().await;
    let legacy_thread = CodexThreadId::parse("legacy-thread").unwrap();
    let bound=owner.command(json!({"command":"call","name":"bind_codex_thread","args":{"thread_id":legacy_thread.as_str()}})).await;
    assert!(bound["ok"].is_object());
    owner
        .command(json!({"command":"legacy_native_fixture"}))
        .await;
    let mut fixture = FixtureProcess::start(owner.teams_delivery(), "legacy/activity").await;
    let activity = fixture.activity("legacy/activity");
    fixture.begin(1, &activity).await;
    assert!(matches!(
        fixture.wait(1).await,
        AdmissionResult::RouteUnavailable
    ));
    assert_eq!(owner.queued().await, 0);
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(owner.queued().await, 0);
    assert_eq!(fixture.snapshot().await.pending_count, 0);
    assert_eq!(
        owner
            .command(json!({"command":"legacy_native_primary_conflict"}))
            .await["kind"],
        "primary_consumer_exists"
    );
    fixture.close().await;
    owner
        .command(json!({"command":"legacy_native_recover"}))
        .await;
    fixture.reopen(owner.teams_delivery()).await;
    assert_eq!(
        fixture.ingress(&activity).await,
        reqwest::StatusCode::ACCEPTED
    );
    let recovered_event = owner.resident_next(&legacy_thread).await.unwrap();
    assert_eq!(
        recovered_event.event["params"]["meta"]["message_id"],
        "legacy/activity"
    );
    fixture.shutdown().await;
    owner.shutdown().await;
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
}
