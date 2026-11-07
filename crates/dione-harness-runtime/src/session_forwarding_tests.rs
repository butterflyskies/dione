use super::{
    HarnessSession,
    session::tests::{peer, receive, wait_for_worker},
};
use camino::Utf8PathBuf;
use dione_attention::{
    delivery::{
        AttentionDeliveryGuard, AttentionDeliveryReceipt, AttentionDescriptor, AttentionDirective,
        AttentionGuardFailure, AttentionReceiptError,
    },
    types::RecordId,
};
use dione_codex::{CodexSessionOptions, LegacyDiscordScope};
use dione_config::DeliveryConfig;
use dione_contracts::{ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef};
use dione_messages::{
    delivery::Delivery,
    events::{AuthorOrigin, MessageCreated, ObservedAuthor, Targeting},
    timestamp::Timestamp,
};
use futures_util::future::BoxFuture;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    net::UnixListener,
    sync::Notify,
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
struct RejectRetainedRecord {
    calls: AtomicUsize,
    checked: Notify,
}
impl AttentionDeliveryGuard for RejectRetainedRecord {
    fn check(&self, descriptor: &AttentionDescriptor<'_>) -> Result<(), AttentionGuardFailure> {
        assert_eq!(descriptor.record().as_str(), "runtime-held");
        assert_eq!(descriptor.source().local().as_numeric().unwrap().get(), 20);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.checked.notify_one();
        Err(AttentionGuardFailure::Invalidated)
    }
    fn receipt<'a>(
        self: Arc<Self>,
        _: &'a RecordId,
        _: AttentionDeliveryReceipt,
    ) -> BoxFuture<'a, Result<(), AttentionReceiptError>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn supplied_attention_guard_rejects_managed_work_before_actual_turn() {
    let directory = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let socket = root.join("app.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let module = super::test_support::module();
    let session = HarnessSession::codex(
        &root,
        CodexSessionOptions::new(
            Some(socket),
            Some("runtime-thread".parse().unwrap()),
            LegacyDiscordScope::new(module.clone()),
        ),
    )
    .await
    .unwrap();
    let connection = session.connection();
    let barrier = Arc::new(Notify::new());
    let (peer, mut received) = peer(listener, barrier.clone());
    let guard = Arc::new(RejectRetainedRecord {
        calls: AtomicUsize::new(0),
        checked: Notify::new(),
    });
    let process = CancellationToken::new();
    let running = session
        .start(&DeliveryConfig::default(), guard.clone(), process.clone())
        .await
        .unwrap();
    wait_for_worker(&connection).await;
    let message = MessageCreated::new(
        MessageRef::new(
            ConversationRef::new(
                module.clone(),
                ConversationId::numeric(10.try_into().unwrap()),
            ),
            MessageId::numeric(20.try_into().unwrap()),
        ),
        ObservedAuthor::new(
            ActorRef::new(module, ActorId::numeric(30.try_into().unwrap())),
            "author".into(),
            AuthorOrigin::DirectHuman,
        ),
        "must remain rejected".into(),
        Targeting::DirectMention,
        Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
    );
    connection
        .deliver(&Delivery::managed(
            message,
            AttentionDirective::Prompt,
            RecordId::new("runtime-held").unwrap(),
            super::test_support::context(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut received).await["method"], "initialize");
    barrier.notify_one();
    timeout(Duration::from_secs(5), guard.checked.notified())
        .await
        .expect("supplied guard sees real managed descriptor");
    assert_eq!(guard.calls.load(Ordering::SeqCst), 1);
    connection
        .deliver(&super::test_support::text("allowed ordinary control"))
        .await
        .unwrap();
    let turn = receive(&mut received).await;
    assert_eq!(turn["method"], "turn/start");
    assert!(turn.to_string().contains("allowed ordinary control"));
    assert!(!turn.to_string().contains("must remain rejected"));
    running.shutdown().await.unwrap();
    assert!(!process.is_cancelled());
    peer.abort();
}

#[tokio::test]
async fn concrete_join_panic_survives_runtime_error_conversion() {
    use std::error::Error as _;
    let source = tokio::spawn(async { panic!("runtime join source witness") })
        .await
        .unwrap_err();
    let converted = super::HarnessSessionError::from(dione_codex::CodexSessionError {
        kind: dione_codex::CodexSessionErrorKind::Join(source),
    });
    assert!(
        matches!(&converted.kind, super::HarnessSessionErrorKind::Join(source) if source.is_panic())
    );
    let source = converted
        .source()
        .expect("runtime conversion retains concrete join cause");
    assert!(
        source
            .downcast_ref::<tokio::task::JoinError>()
            .unwrap()
            .is_panic()
    );
}
