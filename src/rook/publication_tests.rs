use super::*;
use crate::rook::{
    CanonicalPublication, DispatchOutcome, IdempotencyKey, MentionPolicy, PreparedPublication,
    PublicationCapabilities, PublicationDispatch, PublicationErrorKind, PublicationProvider,
    PublicationReceipt, PublicationRejection, PublicationState, PublicationStatus,
    PublicationTarget, PublicationTools, PublicationUncertainty, RejectionPhase, TextPublication,
    private_dir::PrivateStore, publication::COMPLETION_HEADROOM,
};
use serde_json::json;
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::atomic::AtomicUsize,
};

struct BookkeepingDrop(Arc<AtomicBool>);
impl Drop for BookkeepingDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}
struct LogWriter(Arc<Mutex<Vec<u8>>>);
impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Publisher {
    snapshot: PathBuf,
    prepares: AtomicUsize,
    posts: AtomicUsize,
    hold_prepare: AtomicBool,
    hold_dispatch: AtomicBool,
    hold_bookkeeping: AtomicBool,
    bookkeeping_dropped: Arc<AtomicBool>,
    bookkeeping: Arc<Notify>,
    release_bookkeeping: Arc<Notify>,
    preparing: Notify,
    dispatching: Notify,
    release_prepare: Notify,
    release_dispatch: Notify,
    outcome: AtomicU8,
}
impl Publisher {
    fn new(fixture: &Fixture) -> Arc<Self> {
        Arc::new(Self {
            snapshot: fixture
                .root
                .path()
                .join(fixture.id.as_str())
                .join("snapshot.json"),
            prepares: AtomicUsize::new(0),
            posts: AtomicUsize::new(0),
            hold_prepare: AtomicBool::new(false),
            hold_dispatch: AtomicBool::new(false),
            hold_bookkeeping: AtomicBool::new(false),
            bookkeeping_dropped: Arc::new(AtomicBool::new(false)),
            bookkeeping: Arc::new(Notify::new()),
            release_bookkeeping: Arc::new(Notify::new()),
            preparing: Notify::new(),
            dispatching: Notify::new(),
            release_prepare: Notify::new(),
            release_dispatch: Notify::new(),
            outcome: AtomicU8::new(0),
        })
    }
}
impl PublicationProvider for Publisher {
    fn capabilities(&self) -> PublicationCapabilities {
        PublicationCapabilities {
            text: true,
            max_text_bytes: 32768,
            attachments: false,
        }
    }
    fn prepare<'a>(
        &'a self,
        request: &'a CanonicalPublication,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedPublication, PublicationRejection>> + Send + 'a>>
    {
        Box::pin(async move {
            self.prepares.fetch_add(1, Ordering::SeqCst);
            self.preparing.notify_one();
            if self.hold_prepare.load(Ordering::SeqCst) {
                self.release_prepare.notified().await;
            }
            PreparedPublication::new(
                request.clone(),
                "test.text".into(),
                1,
                b"rewritten text".to_vec(),
            )
            .map_err(|_| PublicationRejection::InvalidPreparation)
        })
    }
    fn dispatch<'a>(
        &'a self,
        prepared: &'a PreparedPublication,
    ) -> Pin<Box<dyn Future<Output = PublicationDispatch> + Send + 'a>> {
        Box::pin(async move {
            let bytes = tokio::fs::read(&self.snapshot).await.unwrap();
            let retained: RecipientState = serde_json::from_slice(&bytes).unwrap();
            let record = retained
                .publications
                .iter()
                .find(|record| record.request.event() == prepared.request().event())
                .unwrap();
            assert_eq!(
                record.state,
                PublicationState::Dispatching,
                "POST requires durable dispatch authority"
            );
            assert_eq!(
                record.prepared.as_ref().unwrap().payload(),
                b"rewritten text"
            );
            self.posts.fetch_add(1, Ordering::SeqCst);
            self.dispatching.notify_one();
            if self.hold_dispatch.load(Ordering::SeqCst) {
                self.release_dispatch.notified().await;
            }
            assert_ne!(
                self.outcome.load(Ordering::SeqCst),
                9,
                "private-publication-worker-sentinel"
            );
            if self.outcome.load(Ordering::SeqCst) == 0 {
                let snapshot = self.snapshot.clone();
                let key = prepared.request().event().clone();
                let hold = self.hold_bookkeeping.load(Ordering::SeqCst);
                let entered = self.bookkeeping.clone();
                let release = self.release_bookkeeping.clone();
                let dropped = self.bookkeeping_dropped.clone();
                return PublicationDispatch::with_after_commit(
                    DispatchOutcome::Published(PublicationReceipt {
                        message: event("published", "receipt").key().message().clone(),
                    }),
                    Box::pin(async move {
                        let _drop_signal = BookkeepingDrop(dropped);
                        let bytes = tokio::fs::read(snapshot).await.unwrap();
                        let state: RecipientState = serde_json::from_slice(&bytes).unwrap();
                        assert!(
                            state
                                .publications
                                .iter()
                                .any(|record| record.request.event() == &key
                                    && matches!(record.state, PublicationState::Published { .. })),
                            "bookkeeping requires durable published receipt"
                        );
                        entered.notify_one();
                        if hold {
                            release.notified().await;
                        }
                    }),
                );
            }
            PublicationDispatch::new(match self.outcome.load(Ordering::SeqCst) {
                1 => DispatchOutcome::Unknown(PublicationUncertainty::Transport),
                2 => DispatchOutcome::Refused(PublicationRejection::ProviderRefused),
                4..=7 => DispatchOutcome::Published(boundary_receipt(self.outcome.load(Ordering::SeqCst) >= 6, self.outcome.load(Ordering::SeqCst) % 2 == 1)),
                _ => DispatchOutcome::Published(PublicationReceipt { message: serde_json::from_value(json!({
                    "conversation":{"source":{"provider":"test","slot":"primary"},"local":{"kind":"opaque","value":"wrong-room"}},
                    "local":{"kind":"opaque","value":"wrong-message"}
                })).unwrap() }),
            })
        })
    }
}

fn boundary_receipt(escaped: bool, oversized: bool) -> PublicationReceipt {
    let base = PublicationReceipt {
        message: event("r", "receipt").key().message().clone(),
    };
    let remaining = 4096 - serde_json::to_vec(&base).unwrap().len();
    let mut id = if escaped {
        format!(
            "r{}{}",
            "\n".repeat(remaining / 2),
            "x".repeat(remaining % 2)
        )
    } else {
        format!("r{}", "x".repeat(remaining))
    };
    if oversized {
        id.push('x');
    }
    let receipt = PublicationReceipt {
        message: event(&id, "receipt").key().message().clone(),
    };
    assert_eq!(
        serde_json::to_vec(&receipt).unwrap().len(),
        if oversized { 4097 } else { 4096 }
    );
    receipt
}

async fn open(fixture: &Fixture, publisher: Arc<Publisher>) -> RookService {
    RookService::open_with_publication(
        fixture.root.path().into(),
        fixture.id.clone(),
        fixture.limits.clone(),
        fixture.clock.clone(),
        fixture.access.clone(),
        fixture.transport.clone(),
        publisher,
    )
    .await
    .unwrap()
}
async fn admitted(
    fixture: &Fixture,
    service: &RookService,
    observation: &RetainedObservation,
) -> ClaimGrant {
    service
        .handle
        .configure(
            operator(),
            vec![
                subscription(observation, fixture.clock.now()),
                subscription(observation, fixture.clock.now()),
            ],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                RequestId::new(),
            )
            .await
            .unwrap(),
    )
}
fn request(observation: &RetainedObservation, key: &str) -> TextPublication {
    TextPublication {
        event: observation.key().clone(),
        key: IdempotencyKey::new(key.into()).unwrap(),
        target: PublicationTarget::Reply(observation.key().message().clone()),
        text: "original text".into(),
        mentions: MentionPolicy::SuppressAll,
    }
}
async fn terminal(
    fixture: &Fixture,
    service: &RookService,
    observation: &RetainedObservation,
) -> PublicationStatus {
    for _ in 0..200 {
        if let Some(status) = service
            .handle
            .publication_status(fixture.recipient(), observation.key().clone())
            .await
            .unwrap()
            && !matches!(
                status.outcome,
                PublicationState::Reserved | PublicationState::Dispatching
            )
        {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("publication did not reach a terminal outcome");
}
async fn notified(notification: &Notify) {
    tokio::time::timeout(Duration::from_secs(2), notification.notified())
        .await
        .unwrap();
}

#[tokio::test]
async fn overlapping_subscriptions_and_aliases_share_one_post_through_tools() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_prepare.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("overlap", "authorize another room");
    let grant = admitted(&fixture, &service, &observation).await;
    let tools = PublicationTools::new(fixture.recipient(), service.handle.clone());
    assert_eq!(tools.definitions().len(), 2);
    let first = tools.call(
        "rook_publish_text",
        json!({"grant":grant,"request":request(&observation,"first")}),
    );
    let second = tools.call(
        "rook_publish_text",
        json!({"grant":grant,"request":request(&observation,"second")}),
    );
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first.unwrap()["outcome"]["state"], "reserved");
    assert_eq!(second.unwrap()["outcome"]["state"], "reserved");
    notified(&publisher.preparing).await;
    assert_eq!(publisher.prepares.load(Ordering::SeqCst), 1);
    publisher.release_prepare.notify_one();
    assert!(matches!(
        terminal(&fixture, &service, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    let status = tools
        .call(
            "rook_publication_status",
            json!({"event":observation.key()}),
        )
        .await
        .unwrap();
    assert_eq!(
        status["outcome"]["receipt"]["message"]["local"]["value"],
        "published"
    );
    let same = tools
        .call(
            "rook_publish_text",
            json!({"grant":grant,"request":request(&observation,"first")}),
        )
        .await
        .unwrap();
    assert_eq!(same["outcome"]["state"], "published");
    let state = retained(&fixture).await;
    assert_eq!(state.publications.len(), 1);
    assert_eq!(state.publications[0].aliases.len(), 2);
    assert_eq!(state.publications[0].request.text(), "original text");
    assert_eq!(
        state.publications[0].prepared.as_ref().unwrap().payload(),
        b"rewritten text"
    );
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn changed_request_and_reused_global_key_conflict_before_another_post() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("conflict", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    terminal(&fixture, &service, &observation).await;
    for reuse_key in [true, false] {
        let mut changed = request(&observation, if reuse_key { "key" } else { "new" });
        changed.text = "different".into();
        let error = service
            .handle
            .publish(fixture.recipient(), grant.clone(), changed)
            .await
            .unwrap_err();
        assert!(
            matches!(error.kind, OwnerErrorKind::Publication(error) if matches!(error.kind, PublicationErrorKind::IdempotencyConflict | PublicationErrorKind::PublicationConflict))
        );
    }
    let another = event("another", "body");
    service.handle.admit(another.clone()).await.unwrap();
    let other_grant = granted(
        service
            .handle
            .claim(fixture.recipient(), another.key().clone(), RequestId::new())
            .await
            .unwrap(),
    );
    let error = service
        .handle
        .publish(fixture.recipient(), other_grant, request(&another, "key"))
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind, OwnerErrorKind::Publication(error) if matches!(error.kind, PublicationErrorKind::IdempotencyConflict))
    );
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn canceled_caller_cannot_cancel_an_accepted_reservation_or_post() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("canceled", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    fixture.access.block.store(true, Ordering::SeqCst);
    let handle = service.handle.clone();
    let recipient = fixture.recipient();
    let publication = request(&observation, "key");
    let caller = tokio::spawn(async move { handle.publish(recipient, grant, publication).await });
    notified(&fixture.access.entered).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    fixture.access.release.notify_one();
    assert!(matches!(
        terminal(&fixture, &service, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn blocked_secondary_bookkeeping_cannot_hide_the_durable_receipt() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_bookkeeping.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("bookkeeping", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    notified(&publisher.bookkeeping).await;
    let status = terminal(&fixture, &service, &observation).await;
    assert!(matches!(status.outcome, PublicationState::Published { .. }));
    assert!(!status.durability_pending);
    let retry = service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    assert_eq!(retry, status);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    publisher.release_bookkeeping.notify_one();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn secondary_bookkeeping_deadline_logs_failure_without_releasing_another_post() {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let writer_logs = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .without_time()
        .with_ansi(false)
        .with_writer(move || LogWriter(writer_logs.clone()))
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_bookkeeping.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("bookkeeping-deadline", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    notified(&publisher.bookkeeping).await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    assert!(publisher.bookkeeping_dropped.load(Ordering::SeqCst));
    let logs = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("publication bookkeeping timed out after durable outcome"));
    let status = service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    assert!(matches!(status.outcome, PublicationState::Published { .. }));
    assert!(!status.durability_pending);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn unsupported_provider_invalid_grant_and_injected_principal_fail_before_prepare() {
    let fixture = Fixture::new();
    let ordinary = fixture.open().await;
    let observation = event("auth", "body");
    let grant = admitted(&fixture, &ordinary, &observation).await;
    let error = ordinary
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.kind, OwnerErrorKind::Publication(error) if matches!(error.kind, PublicationErrorKind::Unsupported))
    );
    assert!(!ordinary.handle.publication_capabilities().text);
    ordinary.shutdown().await.unwrap();
    let publisher = Publisher::new(&fixture);
    let service = open(&fixture, publisher.clone()).await;
    let mut forged = grant.clone();
    forged.token = GrantToken::new();
    assert!(matches!(
        service
            .handle
            .publish(fixture.recipient(), forged, request(&observation, "key"))
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::Denied
    ));
    let tools = PublicationTools::new(fixture.recipient(), service.handle.clone());
    assert!(
        tools
            .call(
                "rook_publish_text",
                json!({"grant":grant,"request":request(&observation,"key"),"recipient":"other"})
            )
            .await
            .is_err()
    );
    let wrong = AuthenticatedRecipient::from_authenticated_identity(RecipientId::new());
    assert!(matches!(
        service
            .handle
            .publish(wrong, grant.clone(), request(&observation, "key"))
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::Denied
    ));
    assert_eq!(publisher.prepares.load(Ordering::SeqCst), 0);
    service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    assert!(matches!(
        terminal(&fixture, &service, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_change_during_preparation_refuses_without_disabling_subscription() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_prepare.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("revoked", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    notified(&publisher.preparing).await;
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    publisher.release_prepare.notify_one();
    assert_eq!(
        terminal(&fixture, &service, &observation).await.outcome,
        PublicationState::Rejected {
            phase: RejectionPhase::BeforeDispatch,
            reason: PublicationRejection::AuthorityChanged
        }
    );
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn late_receipt_survives_claim_expiry_settlement_and_reopen() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_dispatch.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("late", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    notified(&publisher.dispatching).await;
    service
        .handle
        .settle(
            fixture.recipient(),
            observation.key().clone(),
            grant.token.clone(),
            RequestId::new(),
            Settlement::NoReply,
        )
        .await
        .unwrap();
    fixture.clock.advance(10);
    publisher.release_dispatch.notify_one();
    assert!(matches!(
        terminal(&fixture, &service, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    service.shutdown().await.unwrap();
    let reopened = open(&fixture, publisher.clone()).await;
    let retry = reopened
        .handle
        .publish(
            fixture.recipient(),
            grant,
            request(&observation, "new-alias"),
        )
        .await
        .unwrap();
    assert!(matches!(retry.outcome, PublicationState::Published { .. }));
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_dispatching_is_unknown_and_never_reposts_with_new_key() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_dispatch.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("crash", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    notified(&publisher.dispatching).await;
    service.crash().await;
    let reopened = open(&fixture, publisher.clone()).await;
    let retry = reopened
        .handle
        .publish(
            fixture.recipient(),
            grant,
            request(&observation, "different-key"),
        )
        .await
        .unwrap();
    assert_eq!(
        retry.outcome,
        PublicationState::Unknown {
            reason: PublicationUncertainty::Restarted
        }
    );
    assert!(!retry.durability_pending);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn restart_reserved_performs_no_automatic_post_then_accepts_explicit_retry() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.hold_prepare.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("reserved", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(
            fixture.recipient(),
            grant.clone(),
            request(&observation, "key"),
        )
        .await
        .unwrap();
    notified(&publisher.preparing).await;
    service.crash().await;
    publisher.hold_prepare.store(false, Ordering::SeqCst);
    let reopened = open(&fixture, publisher.clone()).await;
    reopened.handle.tick().await.unwrap();
    assert_eq!(
        reopened
            .handle
            .publication_status(fixture.recipient(), observation.key().clone())
            .await
            .unwrap()
            .unwrap()
            .outcome,
        PublicationState::Reserved
    );
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
    reopened
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    assert!(matches!(
        terminal(&fixture, &reopened, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_response_wrong_receipt_and_provider_refusal_remain_terminal() {
    for (mode, expected) in [
        (
            1,
            PublicationState::Unknown {
                reason: PublicationUncertainty::Transport,
            },
        ),
        (
            2,
            PublicationState::Rejected {
                phase: RejectionPhase::ProviderRefused,
                reason: PublicationRejection::ProviderRefused,
            },
        ),
        (
            3,
            PublicationState::Unknown {
                reason: PublicationUncertainty::InvalidReceipt,
            },
        ),
    ] {
        let fixture = Fixture::new();
        let publisher = Publisher::new(&fixture);
        publisher.outcome.store(mode, Ordering::SeqCst);
        let service = open(&fixture, publisher.clone()).await;
        let observation = event("uncertain", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        service
            .handle
            .publish(
                fixture.recipient(),
                grant.clone(),
                request(&observation, "key"),
            )
            .await
            .unwrap();
        assert_eq!(
            terminal(&fixture, &service, &observation).await.outcome,
            expected
        );
        fixture.clock.advance(20);
        let retry = service
            .handle
            .publish(fixture.recipient(), grant, request(&observation, "new-key"))
            .await
            .unwrap();
        assert_eq!(retry.outcome, expected);
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn dispatch_is_not_released_until_snapshot_directory_sync_succeeds() {
    for failure in [Failure::BeforeRename, Failure::AfterRename] {
        let fixture = Fixture::new();
        let publisher = Publisher::new(&fixture);
        publisher.hold_prepare.store(true, Ordering::SeqCst);
        let service = open(&fixture, publisher.clone()).await;
        let observation = event("disk-before-post", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        service
            .handle
            .publish(fixture.recipient(), grant, request(&observation, "key"))
            .await
            .unwrap();
        notified(&publisher.preparing).await;
        service.handle.fail_storage(Some(failure)).await.unwrap();
        publisher.release_prepare.notify_one();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            publisher.posts.load(Ordering::SeqCst),
            0,
            "storage failure must precede every POST"
        );
        service.handle.fail_storage(None).await.unwrap();
        service.handle.tick().await.unwrap();
        assert!(matches!(
            terminal(&fixture, &service, &observation).await.outcome,
            PublicationState::Published { .. }
        ));
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn receipt_commit_failure_retains_memory_outcome_without_resending() {
    for failure in [Failure::BeforeRename, Failure::AfterRename] {
        let fixture = Fixture::new();
        let publisher = Publisher::new(&fixture);
        publisher.hold_dispatch.store(true, Ordering::SeqCst);
        let service = open(&fixture, publisher.clone()).await;
        let observation = event("disk-after-post", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        service
            .handle
            .publish(
                fixture.recipient(),
                grant.clone(),
                request(&observation, "key"),
            )
            .await
            .unwrap();
        notified(&publisher.dispatching).await;
        service.handle.fail_storage(Some(failure)).await.unwrap();
        publisher.release_dispatch.notify_one();
        let status = terminal(&fixture, &service, &observation).await;
        assert!(matches!(status.outcome, PublicationState::Published { .. }));
        assert!(status.durability_pending);
        let retry = service
            .handle
            .publish(fixture.recipient(), grant, request(&observation, "key"))
            .await
            .unwrap();
        assert!(matches!(retry.outcome, PublicationState::Published { .. }));
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        service.handle.fail_storage(None).await.unwrap();
        service.handle.tick().await.unwrap();
        assert!(
            !terminal(&fixture, &service, &observation)
                .await
                .durability_pending
        );
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn completion_headroom_survives_capacity_pressure_and_rejects_limit_decrease() {
    for mode in [4, 6] {
        let mut fixture = Fixture::new();
        fixture.limits.snapshot_bytes = 32_768;
        let publisher = Publisher::new(&fixture);
        publisher.hold_dispatch.store(true, Ordering::SeqCst);
        publisher.outcome.store(mode, Ordering::SeqCst);
        let service = open(&fixture, publisher.clone()).await;
        let observation = event("capacity", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        service
            .handle
            .publish(fixture.recipient(), grant, request(&observation, "key"))
            .await
            .unwrap();
        notified(&publisher.dispatching).await;
        let before = retained(&fixture).await;
        let encoded = serde_json::to_vec(&before).unwrap().len();
        let mut reduced = fixture.limits.clone();
        reduced.snapshot_bytes = encoded + COMPLETION_HEADROOM - 1;
        assert!(matches!(
            service
                .handle
                .set_limits(operator(), reduced)
                .await
                .unwrap_err()
                .kind,
            OwnerErrorKind::Capacity
        ));
        let mut inserted = 0;
        for index in 0..100 {
            match service
                .handle
                .admit(event(&format!("fill-{index}"), &"x".repeat(1200)))
                .await
            {
                Ok(AdmissionOutcome::Inserted(_)) => inserted += 1,
                Err(error) => {
                    assert!(matches!(error.kind, OwnerErrorKind::Capacity));
                    break;
                }
                other => panic!("unexpected capacity result: {other:?}"),
            }
        }
        assert!(inserted > 0);
        let full = retained(&fixture).await;
        assert!(
            serde_json::to_vec(&full).unwrap().len() + COMPLETION_HEADROOM
                <= fixture.limits.snapshot_bytes
        );
        publisher.release_dispatch.notify_one();
        let status = terminal(&fixture, &service, &observation).await;
        assert_eq!(
            status.outcome,
            PublicationState::Published {
                receipt: boundary_receipt(mode == 6, false)
            }
        );
        assert!(!status.durability_pending);
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn explicit_v1_migration_preserves_claims_and_requires_v2_ledger() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("migration", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    let original = retained(&fixture).await;
    service.shutdown().await.unwrap();
    {
        let mut storage = PrivateStore::open(fixture.root.path(), fixture.id.as_str()).unwrap();
        storage.write(&original.v1_fixture_bytes()).unwrap();
    }
    let publisher = Publisher::new(&fixture);
    let reopened = open(&fixture, publisher.clone()).await;
    let migrated = retained(&fixture).await;
    assert_eq!(migrated.version, 2);
    assert_eq!(migrated.claims, original.claims);
    assert_eq!(
        migrated.events[0].observation,
        original.events[0].observation
    );
    assert_eq!(migrated.events[0].handling, original.events[0].handling);
    assert_eq!(migrated.events[0].history, original.events[0].history);
    assert!(migrated.publications.is_empty());
    reopened
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "key"))
        .await
        .unwrap();
    assert!(matches!(
        terminal(&fixture, &reopened, &observation).await.outcome,
        PublicationState::Published { .. }
    ));
    reopened.shutdown().await.unwrap();
    let mut missing = serde_json::to_value(migrated).unwrap();
    missing.as_object_mut().unwrap().remove("publications");
    assert!(RecipientState::decode(&serde_json::to_vec(&missing).unwrap()).is_err());
    missing["version"] = json!(3);
    assert!(RecipientState::decode(&serde_json::to_vec(&missing).unwrap()).is_err());
}

#[tokio::test]
async fn oversized_provider_receipt_becomes_unknown_across_reopen_and_alias_retry() {
    for mode in [5, 7] {
        let fixture = Fixture::new();
        let publisher = Publisher::new(&fixture);
        publisher.outcome.store(mode, Ordering::SeqCst);
        let service = open(&fixture, publisher.clone()).await;
        let observation = event("oversized", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        service
            .handle
            .publish(
                fixture.recipient(),
                grant.clone(),
                request(&observation, "key"),
            )
            .await
            .unwrap();
        let expected = PublicationState::Unknown {
            reason: PublicationUncertainty::InvalidReceipt,
        };
        assert_eq!(
            terminal(&fixture, &service, &observation).await.outcome,
            expected
        );
        service.shutdown().await.unwrap();
        let reopened = open(&fixture, publisher.clone()).await;
        let status = reopened
            .handle
            .publish(
                fixture.recipient(),
                grant,
                request(&observation, "new-alias"),
            )
            .await
            .unwrap();
        assert_eq!(status.outcome, expected);
        assert!(!status.durability_pending);
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        reopened.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn migration_capacity_failure_preserves_v1_and_starts_no_provider_work() {
    let mut fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("migration-capacity", &"x".repeat(1600));
    admitted(&fixture, &service, &observation).await;
    let mut original = retained(&fixture).await;
    service.shutdown().await.unwrap();
    for _ in 0..10 {
        let size = original.v1_fixture_bytes().len();
        if original.limits.snapshot_bytes == size {
            break;
        }
        original.limits.snapshot_bytes = size;
    }
    let bytes = original.v1_fixture_bytes();
    assert_eq!(bytes.len(), original.limits.snapshot_bytes);
    assert!(bytes.len() >= 1024);
    fixture.limits = original.limits.clone();
    {
        let mut storage = PrivateStore::open(fixture.root.path(), fixture.id.as_str()).unwrap();
        storage.write(&bytes).unwrap();
    }
    let publisher = Publisher::new(&fixture);
    let result = RookService::open_with_publication(
        fixture.root.path().into(),
        fixture.id.clone(),
        fixture.limits.clone(),
        fixture.clock.clone(),
        fixture.access.clone(),
        fixture.transport.clone(),
        publisher.clone(),
    )
    .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("migration exceeded its retained snapshot limit"),
    };
    assert!(matches!(error.kind, OwnerErrorKind::Capacity));
    assert_eq!(tokio::fs::read(&publisher.snapshot).await.unwrap(), bytes);
    assert_eq!(publisher.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn migration_write_faults_preserve_visible_version_and_never_release_provider() {
    for failure in [Failure::BeforeRename, Failure::AfterRename] {
        let fixture = Fixture::new();
        let service = fixture.open().await;
        let observation = event("migration-write", "body");
        let grant = admitted(&fixture, &service, &observation).await;
        let original = retained(&fixture).await;
        service.shutdown().await.unwrap();
        {
            let mut storage = PrivateStore::open(fixture.root.path(), fixture.id.as_str()).unwrap();
            storage.write(&original.v1_fixture_bytes()).unwrap();
        }
        let publisher = Publisher::new(&fixture);
        let result = RookService::open_with_publication_fault(
            fixture.root.path().into(),
            fixture.id.clone(),
            fixture.limits.clone(),
            fixture.clock.clone(),
            fixture.access.clone(),
            fixture.transport.clone(),
            (publisher.clone(), failure),
        )
        .await;
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("migration unexpectedly ignored write fault"),
        };
        assert!(matches!(error.kind, OwnerErrorKind::Storage(_)));
        let visible: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&publisher.snapshot).await.unwrap()).unwrap();
        assert_eq!(
            visible["version"],
            if failure == Failure::BeforeRename {
                1
            } else {
                2
            }
        );
        assert_eq!(publisher.prepares.load(Ordering::SeqCst), 0);
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
        let reopened = open(&fixture, publisher.clone()).await;
        assert_eq!(retained(&fixture).await.version, 2);
        assert!(
            reopened
                .handle
                .publication_status(fixture.recipient(), observation.key().clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
        reopened
            .handle
            .publish(fixture.recipient(), grant, request(&observation, "key"))
            .await
            .unwrap();
        assert!(matches!(
            terminal(&fixture, &reopened, &observation).await.outcome,
            PublicationState::Published { .. }
        ));
        assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
        reopened.shutdown().await.unwrap();
    }
}

#[test]
fn completion_encoding_fits_reserved_headroom_and_request_limits_are_explicit() {
    let observation = event("bound", "body");
    let (canonical, _) = CanonicalPublication::from_request(request(&observation, "key")).unwrap();
    let receipt = PublicationReceipt {
        message: event(&"x".repeat(3600), "receipt").key().message().clone(),
    };
    assert!(receipt.valid_for(&canonical));
    let outcome = PublicationState::Published { receipt };
    assert!(serde_json::to_vec(&outcome).unwrap().len() < COMPLETION_HEADROOM);
    let oversized = PublicationReceipt {
        message: event(&"x".repeat(5000), "receipt").key().message().clone(),
    };
    assert!(!oversized.valid_for(&canonical));
    let mut invalid = request(&observation, "key");
    invalid.text = "x".repeat(32769);
    assert!(CanonicalPublication::from_request(invalid).is_err());
    assert!(IdempotencyKey::new("bad key".into()).is_err());
}

#[test]
fn prepared_publication_deserialization_enforces_constructor_invariants() {
    let observation = event("prepared", "body");
    let (canonical, _) = CanonicalPublication::from_request(request(&observation, "key")).unwrap();
    let prepared =
        PreparedPublication::new(canonical.clone(), "fixture".into(), 1, vec![1]).unwrap();
    let wire = serde_json::to_value(&prepared).unwrap();
    assert_eq!(
        serde_json::from_value::<PreparedPublication>(wire.clone()).unwrap(),
        prepared
    );
    for (field, invalid) in [
        ("version", serde_json::json!(0)),
        ("codec", serde_json::json!("")),
        ("codec", serde_json::json!("x".repeat(65))),
        ("payload", serde_json::json!(vec![0u8; 65_537])),
    ] {
        let mut invalid_wire = wire.clone();
        invalid_wire[field] = invalid;
        assert!(
            serde_json::from_value::<PreparedPublication>(invalid_wire).is_err(),
            "accepted invalid {field}"
        );
    }
    for (field, invalid) in [
        ("version", serde_json::json!(0)),
        ("text", serde_json::json!("")),
    ] {
        let mut invalid_wire = serde_json::to_value(&canonical).unwrap();
        invalid_wire[field] = invalid;
        assert!(serde_json::from_value::<CanonicalPublication>(invalid_wire.clone()).is_err());
        let mut invalid_prepared = wire.clone();
        invalid_prepared["request"] = invalid_wire;
        assert!(serde_json::from_value::<PreparedPublication>(invalid_prepared).is_err());
    }
}

#[tokio::test]
async fn publication_tool_input_decode_retains_path_category_and_private_cause() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("decode", "private body");
    let grant = admitted(&fixture, &service, &observation).await;
    let tools = PublicationTools::new(fixture.recipient(), service.handle.clone());
    let mut arguments = json!({"grant": grant, "request": request(&observation, "decode")});
    arguments["grant"]["generation"] = json!("private-generation-sentinel");
    let error = tools
        .call("rook_publish_text", arguments)
        .await
        .unwrap_err();
    let OwnerErrorKind::Publication(publication) = &error.kind else {
        panic!("input failure must retain publication category")
    };
    let PublicationErrorKind::Decode(cause) = &publication.kind else {
        panic!("input decoding must not be classified as encoding")
    };
    assert_eq!(cause.path().to_string(), "grant.generation");
    assert!(cause.inner().is_data());
    assert!(!error.to_string().contains("private-generation-sentinel"));
    assert!(
        !publication
            .to_string()
            .contains("private-generation-sentinel")
    );
    use std::error::Error as _;
    assert!(
        publication
            .kind
            .source()
            .unwrap()
            .downcast_ref::<serde_path_to_error::Error<serde_json::Error>>()
            .is_some()
    );
    let mut status_arguments = json!({"event": observation.key()});
    status_arguments["event"]["message"]["conversation"]["source"]["provider"] = json!(123);
    let status_error = tools
        .call("rook_publication_status", status_arguments)
        .await
        .unwrap_err();
    let OwnerErrorKind::Publication(publication) = &status_error.kind else {
        panic!("status input failure must retain publication category")
    };
    let PublicationErrorKind::Decode(cause) = &publication.kind else {
        panic!("status arguments must use input decoding category")
    };
    assert_eq!(
        cause.path().to_string(),
        "event.message.conversation.source.provider"
    );
    assert!(cause.inner().is_data());
    assert!(!status_error.to_string().contains("123"));
    assert_eq!(publisher.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 0);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn publication_worker_panic_keeps_join_cause_through_shutdown() {
    let fixture = Fixture::new();
    let publisher = Publisher::new(&fixture);
    publisher.outcome.store(9, Ordering::SeqCst);
    publisher.hold_dispatch.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("worker", "body");
    let grant = admitted(&fixture, &service, &observation).await;
    service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "worker"))
        .await
        .unwrap();
    publisher.dispatching.notified().await;
    publisher.release_dispatch.notify_one();
    tokio::time::timeout(
        Duration::from_secs(2),
        service.handle.wait_until_stopped_for_test(),
    )
    .await
    .unwrap();
    let error = service.shutdown().await.unwrap_err();
    assert_worker_join(&error, true, false);
    assert!(
        !error
            .to_string()
            .contains("private-publication-worker-sentinel")
    );
}

async fn worker_panic_with_pending_receipt_case(failure: Failure) {
    let mut fixture = Fixture::new();
    fixture.limits.attempts_per_binding = 1;
    fixture.transport.block.store(true, Ordering::SeqCst);
    let publisher = Publisher::new(&fixture);
    publisher.hold_dispatch.store(true, Ordering::SeqCst);
    let service = open(&fixture, publisher.clone()).await;
    let observation = event("primary-worker-pending-receipt", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    let grant = granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                RequestId::new(),
            )
            .await
            .unwrap(),
    );
    service
        .handle
        .publish(
            fixture.recipient(),
            grant,
            request(&observation, "one-post"),
        )
        .await
        .unwrap();
    notified(&publisher.dispatching).await;
    service.handle.fail_storage(Some(failure)).await.unwrap();
    publisher.release_dispatch.notify_one();
    // The completion overlay is a retained-memory barrier, not a durable commit.
    let status = terminal(&fixture, &service, &observation).await;
    assert!(matches!(status.outcome, PublicationState::Published { .. }));
    assert!(status.durability_pending);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    // Only release the distinct wake panic after the receipt is retained and blocked.
    fixture.transport.outcome.store(2, Ordering::SeqCst);
    fixture.transport.release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(2),
        service.handle.wait_until_stopped_for_test(),
    )
    .await
    .unwrap();
    let error = service.shutdown().await.unwrap_err();
    assert_worker_join(&error, true, false);
    assert!(!error.to_string().contains("private-wake-worker-sentinel"));
    // The injected disk failure stays active until the worker is joined by shutdown.
    let visible = retained(&fixture).await;
    assert_eq!(visible.publications.len(), 1);
    if failure == Failure::BeforeRename {
        assert!(matches!(
            visible.publications[0].state,
            PublicationState::Dispatching
        ));
    } else {
        assert!(matches!(
            visible.publications[0].state,
            PublicationState::Published { .. }
        ));
    }
    let reopened = open(&fixture, publisher.clone()).await;
    let status = reopened
        .handle
        .publication_status(fixture.recipient(), observation.key().clone())
        .await
        .unwrap()
        .unwrap();
    if failure == Failure::BeforeRename {
        assert!(matches!(
            status.outcome,
            PublicationState::Unknown {
                reason: PublicationUncertainty::Restarted
            }
        ));
    } else {
        assert!(matches!(status.outcome, PublicationState::Published { .. }));
    }
    assert!(!status.durability_pending);
    assert_eq!(publisher.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    assert!(fixture.sent.try_recv().is_err());
    reopened.shutdown().await.unwrap();
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn worker_join_survives_pending_receipt_before_rename_retry_failure() {
    worker_panic_with_pending_receipt_case(Failure::BeforeRename).await;
}
#[tokio::test]
async fn worker_join_survives_pending_receipt_after_rename_retry_failure() {
    worker_panic_with_pending_receipt_case(Failure::AfterRename).await;
}

#[tokio::test]
async fn primary_publication_panic_survives_later_shutdown_wake_drop_panic() {
    let mut fixture = Fixture::new();
    fixture.limits.attempts_per_binding = 1;
    let transport = Arc::new(ShutdownDropTransport {
        entered: Notify::new(),
        dropped: Arc::new(AtomicBool::new(false)),
        calls: AtomicUsize::new(0),
        panic_on_drop: true,
    });
    let publisher = Publisher::new(&fixture);
    publisher.outcome.store(9, Ordering::SeqCst);
    publisher.hold_dispatch.store(true, Ordering::SeqCst);
    let service = RookService::open_with_publication(
        fixture.root.path().into(),
        fixture.id.clone(),
        fixture.limits.clone(),
        fixture.clock.clone(),
        fixture.access.clone(),
        transport.clone(),
        publisher.clone(),
    )
    .await
    .unwrap();
    let observation = event("primary-publication-late-drop", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    service.handle.tick().await.unwrap();
    notified(&transport.entered).await;
    let grant = granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                RequestId::new(),
            )
            .await
            .unwrap(),
    );
    service
        .handle
        .publish(fixture.recipient(), grant, request(&observation, "primary"))
        .await
        .unwrap();
    notified(&publisher.dispatching).await;
    publisher.release_dispatch.notify_one();
    tokio::time::timeout(
        Duration::from_secs(2),
        service.handle.wait_until_stopped_for_test(),
    )
    .await
    .unwrap();
    let error = service.shutdown().await.unwrap_err();
    assert_worker_join(&error, true, false);
    assert!(transport.dropped.load(Ordering::SeqCst));
    assert!(
        !error
            .to_string()
            .contains("private-publication-worker-sentinel")
    );
    assert!(!error.to_string().contains("private-shutdown-drop-sentinel"));
    // Inspect a controlled test payload privately to distinguish the first worker.
    let OwnerErrorKind::WorkerJoin(join) = error.kind else {
        unreachable!()
    };
    let payload = join.into_panic();
    let message = payload.downcast_ref::<String>().unwrap();
    assert!(message.contains("private-publication-worker-sentinel"));
    assert!(!message.contains("private-shutdown-drop-sentinel"));
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    let visible = retained(&fixture).await;
    assert!(matches!(
        visible.publications[0].state,
        PublicationState::Dispatching
    ));
    let reopened = open(&fixture, publisher.clone()).await;
    let status = reopened
        .handle
        .publication_status(fixture.recipient(), observation.key().clone())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        status.outcome,
        PublicationState::Unknown {
            reason: PublicationUncertainty::Restarted
        }
    ));
    assert_eq!(publisher.posts.load(Ordering::SeqCst), 1);
    assert!(fixture.sent.try_recv().is_err());
    reopened.shutdown().await.unwrap();
}
