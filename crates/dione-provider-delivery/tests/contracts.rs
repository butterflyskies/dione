use dione_messages::timestamp::Timestamp;
use dione_provider_delivery::{
    ProviderEventKey, ReplyHandle, ReplyHandleErrorKind, TeamsAdmissionOutcome, TeamsDelivery,
    TeamsDeliveryError, TeamsDeliveryErrorKind, TeamsEventSink, TeamsResidentEvent,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    error::Error,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

fn key() -> ProviderEventKey {
    ProviderEventKey::teams(
        " Tenant/A ".parse().unwrap(),
        "Bot/A".parse().unwrap(),
        "Conversation/A".parse().unwrap(),
        "Activity/A".parse().unwrap(),
    )
}
#[test]
fn external_key_preserves_wire_spelling_and_validates_each_coordinate() {
    let expected = json!({"provider":"teams", "id":{"tenant_id":" Tenant/A ", "bot_id":"Bot/A", "conversation_id":"Conversation/A", "activity_id":"Activity/A"}});
    assert_eq!(serde_json::to_value(key()).unwrap(), expected);
    assert_eq!(
        serde_json::from_value::<ProviderEventKey>(expected.clone()).unwrap(),
        key()
    );
    let mut extended = expected.clone();
    extended["id"]["future_field"] = json!({"kept_by_codec":true});
    assert_eq!(
        serde_json::from_value::<ProviderEventKey>(extended).unwrap(),
        key()
    );
    for field in ["tenant_id", "bot_id", "conversation_id", "activity_id"] {
        for value in ["".to_owned(), "line\nbreak".to_owned(), "x".repeat(513)] {
            let mut bad = expected.clone();
            bad["id"][field] = json!(value);
            assert!(
                serde_json::from_value::<ProviderEventKey>(bad).is_err(),
                "{field}"
            );
        }
        let mut valid = expected.clone();
        valid["id"][field] = json!("x".repeat(512));
        assert_eq!(
            serde_json::to_value(
                serde_json::from_value::<ProviderEventKey>(valid.clone()).unwrap()
            )
            .unwrap(),
            valid
        );
    }
    for (tenant, bot, conversation, activity) in [
        ("other", "Bot/A", "Conversation/A", "Activity/A"),
        (" Tenant/A ", "other", "Conversation/A", "Activity/A"),
        (" Tenant/A ", "Bot/A", "other", "Activity/A"),
        (" Tenant/A ", "Bot/A", "Conversation/A", "other"),
    ] {
        assert_ne!(
            key(),
            ProviderEventKey::teams(
                tenant.parse().unwrap(),
                bot.parse().unwrap(),
                conversation.parse().unwrap(),
                activity.parse().unwrap()
            )
        );
    }
}
#[test]
fn external_handle_preserves_history_and_formats_typed_uuid() {
    for spelling in [
        "teams-ABCDEF0123456789ABCDEF0123456789",
        "teams-abcdef01-2345-6789-abcd-ef0123456789",
    ] {
        let handle = ReplyHandle::parse(spelling).unwrap();
        assert_eq!(handle.as_str(), spelling);
        assert_eq!(
            serde_json::to_string(&handle).unwrap(),
            format!("\"{spelling}\"")
        );
        assert_eq!(
            serde_json::from_value::<ReplyHandle>(json!(spelling)).unwrap(),
            handle
        );
    }
    let id = uuid::Uuid::parse_str("ABCDEF0123456789ABCDEF0123456789").unwrap();
    let canonical: ReplyHandle = id.into();
    assert_eq!(
        canonical.as_str(),
        "teams-abcdef01-2345-6789-abcd-ef0123456789"
    );
    assert_eq!(
        String::from(canonical),
        "teams-abcdef01-2345-6789-abcd-ef0123456789"
    );
    for invalid in ["", "missing-prefix", "teams-bad"] {
        assert!(ReplyHandle::parse(invalid).is_err());
        assert!(serde_json::from_value::<ReplyHandle>(json!(invalid)).is_err());
    }
    let prefix = ReplyHandle::parse("bad").unwrap_err();
    assert!(matches!(prefix.kind, ReplyHandleErrorKind::MissingPrefix));
    assert_eq!(prefix.to_string(), "invalid Teams reply handle");
    let invalid = ReplyHandle::parse("teams-bad").unwrap_err();
    match &invalid.kind {
        ReplyHandleErrorKind::MissingPrefix => panic!("UUID category lost"),
        ReplyHandleErrorKind::InvalidUuid(source) => assert!(!source.to_string().is_empty()),
    }
    assert!(invalid.source().unwrap().source().is_some());
}
fn event() -> TeamsResidentEvent {
    TeamsResidentEvent {
        provider_key: key(),
        content: "literal content".to_owned(),
        message_id: "Activity/A".parse().unwrap(),
        conversation_id: "Conversation/A".parse().unwrap(),
        channel_id: "teams-channel".to_owned(),
        user_id: "Actor/A".parse().unwrap(),
        bot_id: "Bot/A".parse().unwrap(),
        tenant_id: " Tenant/A ".parse().unwrap(),
        reply_handle: ReplyHandle::parse("teams-ABCDEF0123456789ABCDEF0123456789").unwrap(),
        timestamp: Timestamp::parse("2026-11-05T11:00:00Z").unwrap(),
    }
}
struct Sink {
    name: String,
    captured: Arc<Mutex<Vec<Value>>>,
    outcomes: Mutex<VecDeque<TeamsAdmissionOutcome>>,
}
impl TeamsEventSink for Sink {
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.captured.lock().unwrap().push(json!({"sink":self.name,"provider_key":event.provider_key,"content":event.content,"message_id":event.message_id,"conversation_id":event.conversation_id,"channel_id":event.channel_id,"user_id":event.user_id,"bot_id":event.bot_id,"tenant_id":event.tenant_id,"reply_handle":event.reply_handle,"timestamp":event.timestamp}));
            Ok(self.outcomes.lock().unwrap().pop_front().unwrap())
        })
    }
}
fn storage_error() -> TeamsDeliveryError {
    TeamsDeliveryError {
        kind: TeamsDeliveryErrorKind::Storage(Box::new(std::io::Error::other(
            "literal storage cause",
        ))),
    }
}
#[tokio::test]
async fn cloned_delivery_uses_borrowed_send_sink_and_retains_event_and_outcomes() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    drop(sender);
    let lost = receiver.await.unwrap_err();
    let delivery = TeamsDelivery::new(Sink {
        name: "shared sink".to_owned(),
        captured: captured.clone(),
        outcomes: Mutex::new(VecDeque::from([
            TeamsAdmissionOutcome::Committed,
            TeamsAdmissionOutcome::Duplicate,
            TeamsAdmissionOutcome::CommittedDurabilityUncertain(storage_error()),
            TeamsAdmissionOutcome::CompletionUnknown(TeamsDeliveryError {
                kind: TeamsDeliveryErrorKind::ResponseLost(lost),
            }),
        ])),
    });
    let cloned = delivery.clone();
    for (index, capability) in [&delivery, &cloned, &delivery, &cloned]
        .into_iter()
        .enumerate()
    {
        match assert_send(capability.deliver(event())).await.unwrap() {
            TeamsAdmissionOutcome::Committed => assert_eq!(index, 0),
            TeamsAdmissionOutcome::Duplicate => assert_eq!(index, 1),
            TeamsAdmissionOutcome::CommittedDurabilityUncertain(error) => {
                assert_eq!(index, 2);
                assert_eq!(
                    error.source().unwrap().source().unwrap().to_string(),
                    "literal storage cause"
                );
            }
            TeamsAdmissionOutcome::CompletionUnknown(error) => {
                assert_eq!(index, 3);
                assert!(matches!(
                    error.kind,
                    TeamsDeliveryErrorKind::ResponseLost(_)
                ));
            }
        }
    }
    let records = captured.lock().unwrap();
    assert_eq!(records.len(), 4);
    let expected = json!({"sink":"shared sink","provider_key":{"provider":"teams","id":{"tenant_id":" Tenant/A ","bot_id":"Bot/A","conversation_id":"Conversation/A","activity_id":"Activity/A"}},"content":"literal content","message_id":"Activity/A","conversation_id":"Conversation/A","channel_id":"teams-channel","user_id":"Actor/A","bot_id":"Bot/A","tenant_id":" Tenant/A ","reply_handle":"teams-ABCDEF0123456789ABCDEF0123456789","timestamp":"2026-11-05T11:00:00+00:00"});
    for actual in records.iter() {
        assert_eq!(actual, &expected);
    }
}
#[tokio::test]
async fn classified_delivery_errors_retain_distinct_original_sources() {
    let task = tokio::spawn(std::future::pending::<()>());
    task.abort();
    let joined = task.await.unwrap_err();
    let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
    drop(sender);
    let received = receiver.await.unwrap_err();
    for (kind, expected) in [
        (
            TeamsDeliveryErrorKind::CompletionUnknown(joined),
            "resident admission completion is unknown",
        ),
        (
            TeamsDeliveryErrorKind::ResponseLost(received),
            "resident admission completion response was lost",
        ),
        (
            TeamsDeliveryErrorKind::RouteUnavailable(Box::new(std::io::Error::other(
                "route cause",
            ))),
            "resident route unavailable",
        ),
        (
            TeamsDeliveryErrorKind::ReplayCapacity(Box::new(std::io::Error::other(
                "capacity cause",
            ))),
            "resident replay capacity exhausted",
        ),
        (
            TeamsDeliveryErrorKind::Storage(Box::new(std::io::Error::other("storage cause"))),
            "resident storage failed",
        ),
    ] {
        let error = TeamsDeliveryError { kind };
        assert_eq!(error.to_string(), expected);
        assert!(error.source().unwrap().source().is_some());
        match error.kind {
            TeamsDeliveryErrorKind::CompletionUnknown(source) => assert!(source.is_cancelled()),
            TeamsDeliveryErrorKind::ResponseLost(source) => {
                assert_eq!(source.to_string(), "channel closed")
            }
            TeamsDeliveryErrorKind::RouteUnavailable(source) => {
                assert_eq!(source.to_string(), "route cause")
            }
            TeamsDeliveryErrorKind::ReplayCapacity(source) => {
                assert_eq!(source.to_string(), "capacity cause")
            }
            TeamsDeliveryErrorKind::Storage(source) => {
                assert_eq!(source.to_string(), "storage cause")
            }
        }
    }
}

struct RejectedSink;
impl TeamsEventSink for RejectedSink {
    fn deliver<'a>(
        &'a self,
        _: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async { Err(storage_error()) })
    }
}
#[tokio::test]
async fn delivery_forwards_rejection_with_original_cause() {
    let delivery = TeamsDelivery::new(RejectedSink);
    let error = delivery
        .deliver(event())
        .await
        .err()
        .expect("sink rejection");
    assert_eq!(error.to_string(), "resident storage failed");
    assert_eq!(
        error.source().unwrap().source().unwrap().to_string(),
        "literal storage cause"
    );
    match error.kind {
        TeamsDeliveryErrorKind::Storage(source) => assert_eq!(
            source.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::Other
        ),
        TeamsDeliveryErrorKind::CompletionUnknown(_)
        | TeamsDeliveryErrorKind::ResponseLost(_)
        | TeamsDeliveryErrorKind::RouteUnavailable(_)
        | TeamsDeliveryErrorKind::ReplayCapacity(_) => {
            panic!("sink rejection classification changed")
        }
    }
}
