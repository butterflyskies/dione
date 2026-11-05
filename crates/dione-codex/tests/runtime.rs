use dione_codex::{CodexSession, CodexSessionOptions, CodexThreadId, LegacyDiscordScope};
use dione_contracts::{
    ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef, ModuleInstanceId,
    ModuleSlotId, ProviderId,
};
use dione_harness::AttentionPolicy;
use dione_messages::{
    delivery::{Delivery, EvidenceVisibility, PresentationContext},
    events::{AuthorOrigin, MessageCreated, ObservedAuthor, OrdinaryEvent, Targeting},
    timestamp::Timestamp,
};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn public_session_routes_binding_and_preserves_actual_pull_delivery() {
    let directory = tempfile::tempdir().unwrap();
    let path = camino::Utf8Path::from_path(directory.path()).unwrap();
    let module = ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    );
    let session = CodexSession::prepare(
        path,
        CodexSessionOptions::new(None, None, LegacyDiscordScope::new(module.clone())),
    )
    .await
    .unwrap();
    let connection = session.connection();
    assert_eq!(connection.attention_policy(), AttentionPolicy::SafeTurn);
    let protocol = connection.protocol();
    assert!(
        protocol
            .call("bind_codex_thread", &json!({"thread_id":"bad space"}))
            .await
            .unwrap()
            .is_err()
    );
    let bound = protocol
        .call("bind_codex_thread", &json!({"thread_id":"public-thread"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound, json!({"bound":true,"thread_id":"public-thread"}));
    assert!(
        protocol
            .call("not-a-codex-tool", &json!({}))
            .await
            .is_none()
    );
    let queue = session.admission();
    let reader = queue
        .register_consumer("public reader".into(), Duration::from_secs(60), true, true)
        .await
        .unwrap()
        .consumer_id;
    let source = MessageRef::new(
        ConversationRef::new(
            module.clone(),
            ConversationId::numeric(10.try_into().unwrap()),
        ),
        MessageId::numeric(20.try_into().unwrap()),
    );
    let author = ObservedAuthor::new(
        ActorRef::new(module, ActorId::numeric(30.try_into().unwrap())),
        "external".into(),
        AuthorOrigin::DirectHuman,
    );
    let delivery = Delivery::single(
        OrdinaryEvent::Message(Box::new(MessageCreated::new(
            source,
            author,
            "external observation".into(),
            Targeting::Ambient,
            Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
        ))),
        PresentationContext::new(None, EvidenceVisibility::Enabled),
    );
    connection.deliver(&delivery).await.unwrap();
    assert_eq!(queue.status().await.queued, 1);
    let leased = queue
        .next_event(&reader, Duration::ZERO, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.event["params"]["content"], "external observation");
    assert_eq!(leased.event["params"]["meta"]["message_id"], "20");
    queue
        .acknowledge(&reader, &leased.delivery_token)
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 0);
}

#[test]
fn persisted_thread_validation_retains_constructor_policy() {
    assert_eq!(CodexThreadId::parse(" thread ").unwrap().as_str(), "thread");
    assert_eq!(
        serde_json::from_value::<CodexThreadId>(json!("thread"))
            .unwrap()
            .as_str(),
        "thread"
    );
    for value in [
        json!(" thread "),
        json!(""),
        json!("bad space"),
        json!(0),
        json!({"thread":"thread"}),
    ] {
        assert!(serde_json::from_value::<CodexThreadId>(value).is_err());
    }
}
