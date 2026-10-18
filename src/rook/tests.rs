use super::{
    model::*,
    observation::{NativeSupplement, RetainedObservation},
    owner::{Authorizer, Clock, EventDisposition, RequiredAction, RookService, WakeTransport},
    private_dir::Failure,
};
use crate::core::{
    ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef, ModuleInstanceId,
    ModuleSlotId, ProviderId,
    context::{
        Availability, ContextMetadata, ContextObservation, ObservationExtent, ObservationOrigin,
        SenderObservation, UnavailableReason,
    },
    observation::{MessageObservation, MessageOccurrence},
};
use chrono::{DateTime, Utc};
use std::{
    collections::BTreeSet,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use tokio::sync::{Notify, mpsc};

#[test]
fn retained_decode_keeps_typed_field_path_without_exposing_private_values() {
    use std::error::Error;
    let error = RecipientState::decode(br#"{"version":2,"recipient":123}"#)
        .err()
        .unwrap();
    let mut source = error.source();
    let mut decoded = None;
    while let Some(cause) = source {
        if let Some(cause) = cause.downcast_ref::<serde_path_to_error::Error<serde_json::Error>>() {
            decoded = Some(cause);
            break;
        }
        source = cause.source();
    }
    assert_eq!(
        decoded
            .expect("typed decode cause must survive")
            .path()
            .to_string(),
        "recipient"
    );
    assert!(!error.to_string().contains("123"));
}

#[test]
fn retained_decode_keeps_typed_trailing_input_cause_and_sanitized_display() {
    use std::error::Error;
    let error = RecipientState::decode(br#"{"version":2} private-trailing-sentinel"#)
        .err()
        .unwrap();
    let mut source = error.source();
    let mut trailing = false;
    while let Some(cause) = source {
        trailing |= cause
            .downcast_ref::<serde_json::Error>()
            .is_some_and(|cause| cause.is_syntax());
        source = cause.source();
    }
    assert!(trailing, "typed trailing-input JSON cause must survive");
    assert!(!error.to_string().contains("private-trailing-sentinel"));
}

#[path = "context_tests.rs"]
mod context_tests;
#[path = "publication_tests.rs"]
mod publication_tests;

struct TestClock(Mutex<DateTime<Utc>>);
impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        *self.0.lock().unwrap()
    }
}
impl TestClock {
    fn advance(&self, seconds: i64) {
        *self.0.lock().unwrap() += chrono::Duration::seconds(seconds);
    }
}
struct Access {
    decision: AtomicU8,
    block: AtomicBool,
    entered: Notify,
    release: Notify,
}
impl Authorizer for Access {
    fn check<'a>(
        &'a self,
        _: &'a RecipientId,
        _: &'a RetainedObservation,
    ) -> Pin<Box<dyn Future<Output = AccessDecision> + Send + 'a>> {
        Box::pin(async move {
            if self.block.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            match self.decision.load(Ordering::SeqCst) {
                0 => AccessDecision::Allowed,
                1 => AccessDecision::Denied,
                _ => AccessDecision::Unavailable,
            }
        })
    }
}
struct Transport {
    sent: mpsc::UnboundedSender<WakeRequest>,
    outcome: AtomicU8,
    block: AtomicBool,
    release: Notify,
}
impl WakeTransport for Transport {
    fn deliver(
        &self,
        request: WakeRequest,
    ) -> Pin<Box<dyn Future<Output = CallbackOutcome> + Send + '_>> {
        Box::pin(async move {
            self.sent.send(request).unwrap();
            if self.block.load(Ordering::SeqCst) {
                self.release.notified().await;
            }
            match self.outcome.load(Ordering::SeqCst) {
                0 => CallbackOutcome::Accepted,
                2 => panic!("private-wake-worker-sentinel"),
                _ => CallbackOutcome::Failed(DeliveryFailure::Rejected),
            }
        })
    }
}
struct Fixture {
    root: TempDir,
    id: RecipientId,
    limits: Limits,
    clock: Arc<TestClock>,
    access: Arc<Access>,
    transport: Arc<Transport>,
    sent: mpsc::UnboundedReceiver<WakeRequest>,
}
impl Fixture {
    fn new() -> Self {
        let (sent, received) = mpsc::unbounded_channel();
        Self {
            root: TempDir::new().unwrap(),
            id: RecipientId::new(),
            limits: Limits {
                claim_wait_seconds: 2,
                claim_seconds: 3,
                retry_seconds: 1,
                ..Limits::default()
            },
            clock: Arc::new(TestClock(Mutex::new(
                "2026-10-14T12:00:00Z".parse().unwrap(),
            ))),
            access: Arc::new(Access {
                decision: AtomicU8::new(0),
                block: AtomicBool::new(false),
                entered: Notify::new(),
                release: Notify::new(),
            }),
            transport: Arc::new(Transport {
                sent,
                outcome: AtomicU8::new(0),
                block: AtomicBool::new(false),
                release: Notify::new(),
            }),
            sent: received,
        }
    }
    async fn open(&self) -> RookService {
        RookService::open(
            self.root.path().into(),
            self.id.clone(),
            self.limits.clone(),
            self.clock.clone(),
            self.access.clone(),
            self.transport.clone(),
        )
        .await
        .unwrap()
    }
    fn recipient(&self) -> AuthenticatedRecipient {
        AuthenticatedRecipient::from_authenticated_identity(self.id.clone())
    }
    async fn next(&mut self) -> WakeRequest {
        tokio::time::timeout(Duration::from_secs(2), self.sent.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn settled_callback(&self, service: &RookService) {
        // A status command queued after a send can precede its completion. Wait for the durable receipt.
        for _ in 0..50 {
            let bytes = tokio::fs::read(
                self.root
                    .path()
                    .join(self.id.as_str())
                    .join("snapshot.json"),
            )
            .await
            .unwrap();
            let state: RecipientState = serde_json::from_slice(&bytes).unwrap();
            if state.events.iter().any(|e| e.accepted_deadline.is_some()) {
                return;
            }
            service.handle.tick().await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("callback receipt was not committed");
    }
}
fn event(id: &str, content: &str) -> RetainedObservation {
    let source = ModuleInstanceId::new(
        ProviderId::new("test").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    );
    let room = ConversationRef::new(source.clone(), ConversationId::opaque("room").unwrap());
    let observation = MessageObservation::new(
        ContextObservation::new(
            MessageRef::new(room, MessageId::opaque(id).unwrap()),
            SenderObservation::new(
                ActorRef::new(source, ActorId::opaque("author").unwrap()),
                "author".into(),
                Availability::Known(false),
                Availability::Unavailable(UnavailableReason::NotReported),
            ),
            content.into(),
            "2026-10-14T11:59:00Z".parse().unwrap(),
            ContextMetadata::new(
                Availability::Unavailable(UnavailableReason::NotReported),
                Availability::Known(None),
                Availability::Unavailable(UnavailableReason::NotReported),
                Availability::Known(Vec::new()),
                ObservationExtent::new(ObservationOrigin::GatewayCreate),
            ),
        ),
        MessageOccurrence::Created,
    )
    .unwrap();
    RetainedObservation::capture(&observation)
}
fn subscription(event: &RetainedObservation, now: DateTime<Utc>) -> Subscription {
    Subscription {
        id: SubscriptionId::new(),
        enabled: true,
        rooms: BTreeSet::from([event.conversation().clone()]),
        senders: SenderFilter::EveryoneInRoom,
        priority: 0,
        binding: BindingState::Verified {
            generation: BindingGeneration::new(1),
            refresh_before: now + chrono::Duration::hours(1),
        },
    }
}
fn operator() -> AuthorizedOperator {
    AuthorizedOperator::from_authenticated_operator()
}
fn granted(value: ClaimOutcome) -> ClaimGrant {
    match value {
        ClaimOutcome::Granted(grant) => grant,
        _ => panic!("expected claim grant"),
    }
}

async fn retained(fixture: &Fixture) -> RecipientState {
    let bytes = tokio::fs::read(
        fixture
            .root
            .path()
            .join(fixture.id.as_str())
            .join("snapshot.json"),
    )
    .await
    .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn wait_for_receipts(fixture: &Fixture, predicate: impl Fn(&RecipientState) -> bool) {
    for _ in 0..100 {
        if predicate(&retained(fixture).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("expected durable callback receipt");
}

#[tokio::test]
async fn interrupted_final_attempt_parks_without_refund_and_budget_two_retries_once() {
    for budget in [1, 2] {
        let mut fixture = Fixture::new();
        fixture.limits.attempts_per_binding = budget;
        fixture.transport.block.store(true, Ordering::SeqCst);
        let service = fixture.open().await;
        let observation = event("interrupted", "body");
        service
            .handle
            .configure(
                operator(),
                vec![subscription(&observation, fixture.clock.now())],
            )
            .await
            .unwrap();
        service.handle.admit(observation).await.unwrap();
        service.handle.tick().await.unwrap();
        fixture.next().await;
        service.shutdown().await.unwrap();
        fixture.transport.block.store(false, Ordering::SeqCst);
        let reopened = fixture.open().await;
        reopened.handle.tick().await.unwrap();
        if budget == 1 {
            for _ in 0..3 {
                reopened.handle.tick().await.unwrap();
            }
            let status = reopened.handle.status(fixture.recipient()).await.unwrap();
            assert_eq!(
                status.events[0].disposition,
                EventDisposition::Parked(ParkReason::DeliveryAttemptsExhausted)
            );
            assert_eq!(status.events[0].attempts[0].1, 1);
            assert_eq!(
                status.events[0].attempts[0].2,
                Some(DeliveryFailure::Restarted)
            );
            assert_eq!(status.enabled_subscriptions, 1);
            assert!(fixture.sent.try_recv().is_err());
        } else {
            fixture.next().await;
            wait_for_receipts(&fixture, |state| {
                state.events[0].accepted_deadline.is_some()
            })
            .await;
            let status = reopened.handle.status(fixture.recipient()).await.unwrap();
            assert_eq!(status.events[0].attempts[0].1, 2);
            assert!(fixture.sent.try_recv().is_err());
        }
        reopened.shutdown().await.unwrap();
        let again = fixture.open().await;
        again.handle.tick().await.unwrap();
        assert_eq!(
            again
                .handle
                .status(fixture.recipient())
                .await
                .unwrap()
                .events[0]
                .attempts[0]
                .1,
            budget
        );
        assert!(fixture.sent.try_recv().is_err());
        again.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_recovery_binding_uses_alternate_then_parks_finitely() {
    let mut fixture = Fixture::new();
    fixture.limits.attempts_per_binding = 2;
    let service = fixture.open().await;
    let observation = event("overlap", "body");
    let mut first = subscription(&observation, fixture.clock.now());
    first.priority = 0;
    let mut second = subscription(&observation, fixture.clock.now());
    second.priority = 1;
    service
        .handle
        .configure(operator(), vec![first.clone(), second.clone()])
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    service.handle.tick().await.unwrap();
    fixture.next().await;
    wait_for_receipts(&fixture, |state| {
        state.events[0]
            .links
            .iter()
            .all(|link| matches!(link.wake, WakeState::Accepted))
    })
    .await;
    fixture.transport.outcome.store(1, Ordering::SeqCst);
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    assert_eq!(fixture.next().await.subscription, first.id);
    wait_for_receipts(&fixture, |state| {
        state.events[0].links.iter().any(|link| {
            link.subscription == first.id
                && link.attempts == 2
                && link.last_failure == Some(DeliveryFailure::Rejected)
        })
    })
    .await;
    service.handle.tick().await.unwrap();
    assert_eq!(fixture.next().await.subscription, second.id);
    wait_for_receipts(&fixture, |state| {
        matches!(
            state.events[0].handling,
            Handling::Parked {
                reason: ParkReason::DeliveryAttemptsExhausted
            }
        ) && state.events[0]
            .links
            .iter()
            .all(|link| link.attempts == 2 && link.last_failure == Some(DeliveryFailure::Rejected))
    })
    .await;
    service.handle.tick().await.unwrap();
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(
        status.events[0].disposition,
        EventDisposition::Parked(ParkReason::DeliveryAttemptsExhausted)
    );
    assert!(
        status.events[0]
            .attempts
            .iter()
            .all(|(_, attempts, _)| *attempts == 2)
    );
    assert_eq!(status.enabled_subscriptions, 2);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn exhausted_verified_binding_exposes_expired_alternate_refresh_across_reopen() {
    let mut fixture = Fixture::new();
    fixture.limits.attempts_per_binding = 2;
    let service = fixture.open().await;
    let observation = event("mixed-eligibility", "body");
    let first = subscription(&observation, fixture.clock.now());
    let mut second = subscription(&observation, fixture.clock.now());
    second.priority = 1;
    second.binding = BindingState::Verified {
        generation: BindingGeneration::new(1),
        refresh_before: fixture.clock.now() + chrono::Duration::seconds(2),
    };
    service
        .handle
        .configure(operator(), vec![first.clone(), second.clone()])
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    service.handle.tick().await.unwrap();
    fixture.next().await;
    wait_for_receipts(&fixture, |state| {
        state.events[0]
            .links
            .iter()
            .all(|link| matches!(link.wake, WakeState::Accepted))
    })
    .await;
    fixture.transport.outcome.store(1, Ordering::SeqCst);
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    assert_eq!(fixture.next().await.subscription, first.id);
    wait_for_receipts(&fixture, |state| {
        state.events[0].links.iter().any(|link| {
            link.subscription == first.id && matches!(link.wake, WakeState::Exhausted { .. })
        })
    })
    .await;
    let mut running = service;
    for _ in 0..2 {
        for _ in 0..3 {
            fixture.clock.advance(1);
            running.handle.tick().await.unwrap();
            let status = running.handle.status(fixture.recipient()).await.unwrap();
            let event = &status.events[0];
            assert_eq!(event.disposition, EventDisposition::Waiting);
            assert_eq!(event.required_action, RequiredAction::RefreshBinding);
            assert_eq!(status.enabled_subscriptions, 2);
            assert!(
                event
                    .attempts
                    .iter()
                    .any(|(id, attempts, failure)| id == &first.id
                        && *attempts == 2
                        && *failure == Some(DeliveryFailure::Rejected))
            );
            assert!(
                event
                    .attempts
                    .iter()
                    .any(|(id, attempts, _)| id == &second.id && *attempts == 1)
            );
            assert!(fixture.sent.try_recv().is_err());
        }
        running.shutdown().await.unwrap();
        running = fixture.open().await;
    }
    running.shutdown().await.unwrap();
    assert!(fixture.sent.try_recv().is_err());
}

#[tokio::test]
async fn recovery_binding_lease_expiry_selects_current_alternate_without_configuration_change() {
    let mut fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("expiry", "body");
    let mut first = subscription(&observation, fixture.clock.now());
    first.binding = BindingState::Verified {
        generation: BindingGeneration::new(1),
        refresh_before: fixture.clock.now() + chrono::Duration::seconds(5),
    };
    first.priority = 0;
    let mut second = subscription(&observation, fixture.clock.now());
    second.priority = 1;
    service
        .handle
        .configure(operator(), vec![first.clone(), second.clone()])
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    service.handle.tick().await.unwrap();
    fixture.next().await;
    wait_for_receipts(&fixture, |state| {
        state.events[0]
            .links
            .iter()
            .all(|link| matches!(link.wake, WakeState::Accepted))
    })
    .await;
    fixture.transport.outcome.store(1, Ordering::SeqCst);
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    assert_eq!(fixture.next().await.subscription, first.id);
    wait_for_receipts(&fixture, |state| {
        state.events[0].links.iter().any(|link| {
            link.subscription == first.id && matches!(link.wake, WakeState::Retry { .. })
        })
    })
    .await;
    fixture.transport.outcome.store(0, Ordering::SeqCst);
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    assert_eq!(fixture.next().await.subscription, second.id);
    wait_for_receipts(&fixture, |state| {
        state.events[0].accepted_deadline.is_some()
    })
    .await;
    assert_eq!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .events[0]
            .recovery_round,
        1
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn mandatory_recovery_capacity_failure_keeps_owner_inspectable_across_restart() {
    for history_limit in [true, false] {
        let mut fixture = Fixture::new();
        if history_limit {
            fixture.limits.history = 1;
        }
        let service = fixture.open().await;
        let first = event("expiring", "body");
        let second = event("inspectable", "other body");
        let mut binding = subscription(&first, fixture.clock.now());
        if history_limit {
            binding.binding = BindingState::NeedsRefresh;
        }
        service
            .handle
            .configure(operator(), vec![binding.clone()])
            .await
            .unwrap();
        service.handle.admit(first.clone()).await.unwrap();
        if history_limit {
            service
                .handle
                .claim(fixture.recipient(), first.key().clone(), RequestId::new())
                .await
                .unwrap();
        } else {
            fixture.next().await;
            wait_for_receipts(&fixture, |state| {
                state.events[0].accepted_deadline.is_some()
            })
            .await;
            binding.binding = BindingState::NeedsRefresh;
            service
                .handle
                .configure(operator(), vec![binding])
                .await
                .unwrap();
            assert!(matches!(
                retained(&fixture).await.events[0].handling,
                Handling::Unclaimed
            ));
        }
        service.handle.admit(second).await.unwrap();
        if !history_limit {
            let length = serde_json::to_vec(&retained(&fixture).await).unwrap().len();
            fixture.limits.snapshot_bytes = length;
            service
                .handle
                .set_limits(operator(), fixture.limits.clone())
                .await
                .unwrap();
        }
        fixture.clock.advance(4);
        for _ in 0..3 {
            let _ = service.handle.tick().await;
        }
        let status = service.handle.status(fixture.recipient()).await.unwrap();
        assert!(
            status.capacity_blocked,
            "history_limit={history_limit}: recovery must remain blocked"
        );
        assert_eq!(status.events.len(), 2);
        assert_eq!(status.enabled_subscriptions, 1);
        assert_eq!(
            status.events[0].required_action,
            RequiredAction::IncreaseCapacity
        );
        assert_eq!(
            status.events[1].required_action,
            RequiredAction::RefreshBinding
        );
        service.shutdown().await.unwrap();
        let reopened = fixture.open().await;
        for _ in 0..3 {
            let _ = reopened.handle.tick().await;
        }
        assert!(
            reopened
                .handle
                .status(fixture.recipient())
                .await
                .unwrap()
                .capacity_blocked
        );
        fixture.limits.history = 16;
        fixture.limits.snapshot_bytes = Limits::default().snapshot_bytes;
        reopened
            .handle
            .set_limits(operator(), fixture.limits.clone())
            .await
            .unwrap();
        reopened.handle.tick().await.unwrap();
        let status = reopened.handle.status(fixture.recipient()).await.unwrap();
        assert!(!status.capacity_blocked);
        assert_eq!(status.events[0].recovery_round, 1);
        assert_eq!(status.events.len(), 2);
        reopened.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn corrupted_retained_sender_source_is_refused_on_reopen() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("corrupt", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.shutdown().await.unwrap();
    let path = fixture
        .root
        .path()
        .join(fixture.id.as_str())
        .join("snapshot.json");
    let bytes = tokio::fs::read(&path).await.unwrap();
    let mut wire: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    wire["events"][0]["observation"]["sender"]["reference"]["source"]["slot"] =
        serde_json::json!("another-instance");
    let corrupted = serde_json::to_vec(&wire).unwrap();
    #[cfg(unix)]
    let snapshot_identity = || {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o7777, 0o600);
        assert_eq!(metadata.nlink(), 1);
        (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.mode() & 0o7777,
            metadata.nlink(),
        )
    };
    #[cfg(unix)]
    let original_identity = snapshot_identity();
    tokio::fs::write(&path, &corrupted).await.unwrap();
    #[cfg(unix)]
    assert_eq!(snapshot_identity(), original_identity);
    let result = RookService::open(
        fixture.root.path().into(),
        fixture.id.clone(),
        fixture.limits.clone(),
        fixture.clock.clone(),
        fixture.access.clone(),
        fixture.transport.clone(),
    )
    .await;
    let error = result
        .err()
        .expect("corrupted sender source must be refused");
    let cause = match &error.kind {
        OwnerErrorKind::Decode(cause) => cause,
        _ => panic!("expected typed retained observation decode refusal: {error}"),
    };
    assert!(cause.inner().is_data());
    assert_eq!(cause.path().to_string(), "events[0].observation");
    let expected = super::observation::ObservationError::new(
        super::observation::ObservationErrorKind::SenderSourceMismatch,
    )
    .to_string();
    assert!(cause.inner().to_string().starts_with(&expected));
    assert!(!error.to_string().contains("another-instance"));
    assert_eq!(tokio::fs::read(&path).await.unwrap(), corrupted);
    #[cfg(unix)]
    assert_eq!(snapshot_identity(), original_identity);
}

#[test]
fn public_identity_and_token_decoding_rejects_malformed_authority_values() {
    assert!(serde_json::from_str::<RecipientId>("\"message-author\"").is_err());
    assert!(serde_json::from_str::<RequestId>("\"\"").is_err());
    assert!(serde_json::from_str::<GrantToken>("\"pretend-grant\"").is_err());
    let token = GrantToken::new();
    let encoded = serde_json::to_string(&token).unwrap();
    assert_eq!(serde_json::from_str::<GrantToken>(&encoded).unwrap(), token);
    assert_eq!(format!("{token:?}"), "GrantToken([redacted])");
    assert!(!format!("{token:?}").contains(encoded.trim_matches('"')));
}

#[tokio::test]
async fn overlap_claims_and_restart_keep_one_grant_and_settlement() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let event = event("message", "original");
    let first = subscription(&event, fixture.clock.now());
    let second = subscription(&event, fixture.clock.now());
    service
        .handle
        .configure(operator(), vec![first, second])
        .await
        .unwrap();
    assert_eq!(
        service.handle.admit(event.clone()).await.unwrap(),
        AdmissionOutcome::Inserted(2)
    );
    assert_eq!(
        service.handle.admit(event.clone()).await.unwrap(),
        AdmissionOutcome::Duplicate
    );
    let id = RequestId::new();
    let (one, two) = tokio::join!(
        service
            .handle
            .claim(fixture.recipient(), event.key().clone(), id.clone()),
        service
            .handle
            .claim(fixture.recipient(), event.key().clone(), RequestId::new())
    );
    let grant = granted(one.unwrap());
    assert!(matches!(two.unwrap(), ClaimOutcome::Busy { .. }));
    service.shutdown().await.unwrap();
    let reopened = fixture.open().await;
    assert_eq!(
        reopened
            .handle
            .claim(fixture.recipient(), event.key().clone(), id)
            .await
            .unwrap(),
        ClaimOutcome::Granted(grant.clone())
    );
    let settlement = RequestId::new();
    reopened
        .handle
        .settle(
            fixture.recipient(),
            event.key().clone(),
            grant.token.clone(),
            settlement.clone(),
            Settlement::NoReply,
        )
        .await
        .unwrap();
    reopened
        .handle
        .settle(
            fixture.recipient(),
            event.key().clone(),
            grant.token,
            settlement,
            Settlement::NoReply,
        )
        .await
        .unwrap();
    let status = reopened.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(status.enabled_subscriptions, 2);
    assert_eq!(
        status.events[0].disposition,
        EventDisposition::Settled(Settlement::NoReply)
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn conflicting_evidence_and_request_reuse_do_not_create_work() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let first = event("one", "first");
    let second = event("two", "second");
    service
        .handle
        .configure(operator(), vec![subscription(&first, fixture.clock.now())])
        .await
        .unwrap();
    service.handle.admit(first.clone()).await.unwrap();
    service.handle.admit(second.clone()).await.unwrap();
    assert!(matches!(
        service
            .handle
            .admit(event("one", "changed"))
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::ConflictingRevision
    ));
    let request = RequestId::new();
    service
        .handle
        .claim(fixture.recipient(), first.key().clone(), request.clone())
        .await
        .unwrap();
    assert!(matches!(
        service
            .handle
            .claim(fixture.recipient(), second.key().clone(), request)
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::RequestConflict
    ));
    let stranger = AuthenticatedRecipient::from_authenticated_identity(RecipientId::new());
    assert!(matches!(
        service.handle.status(stranger).await.unwrap_err().kind,
        OwnerErrorKind::Denied
    ));
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_request_still_commits_and_retry_recovers_the_grant() {
    let mut fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    // Admission itself requires Allowed. Settle its initial wake before arming the claim gate.
    service.handle.admit(observation.clone()).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    fixture.settled_callback(&service).await;
    // A retained future retry deadline then keeps drive out of the shared authorizer gate.
    fixture.access.decision.store(2, Ordering::SeqCst);
    service.handle.tick().await.unwrap();
    let request = RequestId::new();
    let deferred = retained(&fixture).await;
    let original_event = deferred
        .events
        .iter()
        .find(|event| event.observation.key() == observation.key())
        .unwrap();
    assert!(matches!(original_event.handling, Handling::Unclaimed));
    assert!(
        original_event
            .access_due
            .is_some_and(|due| due > fixture.clock.now())
    );
    assert!(!deferred.claims.iter().any(|claim| claim.id == request));
    fixture.access.decision.store(0, Ordering::SeqCst);
    fixture.access.block.store(true, Ordering::SeqCst);
    let handle = service.handle.clone();
    let recipient = fixture.recipient();
    let key = observation.key().clone();
    let duplicate = request.clone();
    let pending = tokio::spawn(async move { handle.claim(recipient, key, duplicate).await });
    tokio::time::timeout(Duration::from_secs(2), fixture.access.entered.notified())
        .await
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    fixture.access.release.notify_one();
    // Observe the accepted original command's durable write before sending any retry.
    wait_for_receipts(&fixture, |state| {
        state
            .claims
            .iter()
            .any(|claim| claim.id == request && claim.grant.is_some())
    })
    .await;
    let committed = retained(&fixture).await;
    let history: Vec<_> = committed
        .claims
        .iter()
        .filter(|claim| claim.id == request)
        .collect();
    assert_eq!(history.len(), 1);
    assert_eq!(&history[0].event, observation.key());
    let original_grant = history[0].grant.clone().unwrap();
    let original_event = committed
        .events
        .iter()
        .find(|event| event.observation.key() == observation.key())
        .unwrap();
    let Handling::Claimed {
        request: committed_request,
        grant: committed_grant,
    } = &original_event.handling
    else {
        panic!("accepted original claim must commit before retry")
    };
    assert_eq!(committed_request, &request);
    assert_eq!(committed_grant, &original_grant);
    let grant = granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                request.clone(),
            )
            .await
            .unwrap(),
    );
    assert_eq!(grant, original_grant);
    service.shutdown().await.unwrap();
    let reopened = fixture.open().await;
    assert_eq!(
        reopened
            .handle
            .claim(fixture.recipient(), observation.key().clone(), request)
            .await
            .unwrap(),
        ClaimOutcome::Granted(original_grant)
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_wake_without_run_recovers_once_then_parks_visibly() {
    let mut fixture = Fixture::new();
    fixture.limits.recovery_rounds = 1;
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![
                subscription(&observation, fixture.clock.now()),
                subscription(&observation, fixture.clock.now()),
            ],
        )
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    fixture.settled_callback(&service).await;
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(status.events[0].recovery_round, 1);
    fixture.settled_callback(&service).await;
    fixture.clock.advance(3);
    service.handle.tick().await.unwrap();
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(
        status.events[0].disposition,
        EventDisposition::Parked(ParkReason::ClaimNotStarted)
    );
    assert_eq!(status.enabled_subscriptions, 2);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_claim_and_noop_do_not_disable_subscription() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    let request = RequestId::new();
    let old = granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                request.clone(),
            )
            .await
            .unwrap(),
    );
    fixture.clock.advance(4);
    service.handle.tick().await.unwrap();
    assert_eq!(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), request)
            .await
            .unwrap(),
        ClaimOutcome::RecoveryRequired
    );
    assert!(matches!(
        service
            .handle
            .settle(
                fixture.recipient(),
                observation.key().clone(),
                old.token,
                RequestId::new(),
                Settlement::Handled
            )
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::InvalidClaim
    ));
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(status.enabled_subscriptions, 1);
    assert_eq!(status.events[0].recovery_round, 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn policy_revokes_claim_but_binding_renewal_does_not() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    let mut binding = subscription(&observation, fixture.clock.now());
    service
        .handle
        .configure(operator(), vec![binding.clone()])
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    let request = RequestId::new();
    let grant = granted(
        service
            .handle
            .claim(
                fixture.recipient(),
                observation.key().clone(),
                request.clone(),
            )
            .await
            .unwrap(),
    );
    binding.binding = BindingState::Verified {
        generation: BindingGeneration::new(2),
        refresh_before: fixture.clock.now() + chrono::Duration::hours(2),
    };
    service
        .handle
        .configure(operator(), vec![binding.clone()])
        .await
        .unwrap();
    assert_eq!(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), request)
            .await
            .unwrap(),
        ClaimOutcome::Granted(grant.clone())
    );
    binding.enabled = false;
    service
        .handle
        .configure(operator(), vec![binding])
        .await
        .unwrap();
    assert!(matches!(
        service
            .handle
            .settle(
                fixture.recipient(),
                observation.key().clone(),
                grant.token,
                RequestId::new(),
                Settlement::NoReply
            )
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::Denied
    ));
    service.handle.tick().await.unwrap();
    assert_eq!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .events[0]
            .disposition,
        EventDisposition::Parked(ParkReason::AccessDenied)
    );
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_binding_and_temporary_access_failure_remain_visible() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    let mut binding = subscription(&observation, fixture.clock.now());
    binding.binding = BindingState::NeedsRefresh;
    service
        .handle
        .configure(operator(), vec![binding])
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    service.handle.tick().await.unwrap();
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(
        status.events[0].required_action,
        RequiredAction::RefreshBinding
    );
    assert_eq!(status.events[0].attempts[0].1, 0);
    fixture.access.decision.store(2, Ordering::SeqCst);
    for _ in 0..fixture.limits.access_retries {
        fixture.clock.advance(2);
        service.handle.tick().await.unwrap();
    }
    let status = service.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(
        status.events[0].disposition,
        EventDisposition::Parked(ParkReason::AccessUnavailable)
    );
    assert_eq!(status.enabled_subscriptions, 1);
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_retains_history_and_settled_event_is_inspection_only() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    let replay = RequestId::new();
    assert_eq!(
        service
            .handle
            .replay(operator(), observation.key().clone(), replay.clone())
            .await
            .unwrap(),
        ReplayOutcome::Scheduled { cycle: 1 }
    );
    assert_eq!(
        service
            .handle
            .replay(operator(), observation.key().clone(), replay)
            .await
            .unwrap(),
        ReplayOutcome::Scheduled { cycle: 1 }
    );
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
        .settle(
            fixture.recipient(),
            observation.key().clone(),
            grant.token,
            RequestId::new(),
            Settlement::Handled,
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .handle
            .replay(operator(), observation.key().clone(), RequestId::new())
            .await
            .unwrap(),
        ReplayOutcome::Settled(Settlement::Handled)
    );
    service.shutdown().await.unwrap();
    let bytes = tokio::fs::read(
        fixture
            .root
            .path()
            .join(fixture.id.as_str())
            .join("snapshot.json"),
    )
    .await
    .unwrap();
    let state: RecipientState = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(state.events[0].history.len(), 1);
    assert_eq!(state.replays.len(), 1);
}

#[tokio::test]
async fn dispatching_restart_preserves_spent_attempt_and_does_not_report_success() {
    let mut fixture = Fixture::new();
    fixture.transport.block.store(true, Ordering::SeqCst);
    let service = fixture.open().await;
    let observation = event("message", "body");
    let binding = subscription(&observation, fixture.clock.now());
    service
        .handle
        .configure(operator(), vec![binding])
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    service.handle.tick().await.unwrap();
    fixture.next().await;
    service.shutdown().await.unwrap();
    fixture.access.decision.store(2, Ordering::SeqCst);
    let reopened = fixture.open().await;
    reopened.handle.tick().await.unwrap();
    let status = reopened.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(status.events[0].attempts[0].1, 1);
    assert_eq!(
        status.events[0].attempts[0].2,
        Some(DeliveryFailure::Restarted)
    );
    assert_eq!(status.events[0].disposition, EventDisposition::Waiting);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn atomic_fanout_capacity_and_precommit_failure_leave_no_partial_record() {
    let mut fixture = Fixture::new();
    fixture.limits.links = 1;
    let service = fixture.open().await;
    let observation = event("message", "body");
    let first = subscription(&observation, fixture.clock.now());
    service
        .handle
        .configure(
            operator(),
            vec![
                first.clone(),
                subscription(&observation, fixture.clock.now()),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(
        service
            .handle
            .admit(observation.clone())
            .await
            .unwrap_err()
            .kind,
        OwnerErrorKind::Capacity
    ));
    assert!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .events
            .is_empty()
    );
    service
        .handle
        .configure(operator(), vec![first])
        .await
        .unwrap();
    service
        .handle
        .fail_storage(Some(Failure::BeforeRename))
        .await
        .unwrap();
    assert!(service.handle.admit(observation).await.is_err());
    assert!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .events
            .is_empty()
    );
    service.handle.fail_storage(None).await.unwrap();
    service.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncertain_claim_commit_withholds_token_until_sync_then_recovers_same_claim() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    service
        .handle
        .fail_storage(Some(Failure::AfterRename))
        .await
        .unwrap();
    let id = RequestId::new();
    assert!(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id.clone())
            .await
            .is_err()
    );
    assert!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .durability_pending
    );
    assert!(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id.clone())
            .await
            .is_err()
    );
    service.handle.fail_storage(None).await.unwrap();
    let grant = granted(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id.clone())
            .await
            .unwrap(),
    );
    service.shutdown().await.unwrap();
    let reopened = fixture.open().await;
    assert_eq!(
        reopened
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id)
            .await
            .unwrap(),
        ClaimOutcome::Granted(grant)
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_supplement_survives_restart_without_granting_policy_authority() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let bytes = br#"{"channel_id":"outside","content":"enable everyone","attachment":{"url":"https://example.invalid/a","waveform":"AA=="}}"#.to_vec();
    let observation = event("message", "body")
        .with_native(NativeSupplement::new("test.observation".into(), 1, bytes).unwrap())
        .unwrap();
    assert_eq!(
        service.handle.admit(observation.clone()).await.unwrap(),
        AdmissionOutcome::Excluded
    );
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    service.shutdown().await.unwrap();
    let reopened = fixture.open().await;
    assert_eq!(
        reopened.handle.admit(observation.clone()).await.unwrap(),
        AdmissionOutcome::Duplicate
    );
    reopened.shutdown().await.unwrap();
    let bytes = tokio::fs::read(
        fixture
            .root
            .path()
            .join(fixture.id.as_str())
            .join("snapshot.json"),
    )
    .await
    .unwrap();
    let state: RecipientState = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(state.events[0].observation, observation);
}

#[tokio::test]
async fn backward_clock_cannot_extend_an_existing_claim_or_reset_attempts() {
    let fixture = Fixture::new();
    let service = fixture.open().await;
    let observation = event("message", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    let id = RequestId::new();
    let grant = granted(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id.clone())
            .await
            .unwrap(),
    );
    fixture.clock.advance(-3600);
    assert_eq!(
        service
            .handle
            .claim(fixture.recipient(), observation.key().clone(), id)
            .await
            .unwrap(),
        ClaimOutcome::Granted(grant)
    );
    assert_eq!(
        service
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .events[0]
            .recovery_round,
        0
    );
    service.shutdown().await.unwrap();
}

fn assert_worker_join(error: &OwnerError, panicked: bool, cancelled: bool) {
    use std::error::Error as _;
    let OwnerErrorKind::WorkerJoin(join) = &error.kind else {
        panic!("supervised worker failure must retain typed join category")
    };
    assert_eq!(join.is_panic(), panicked);
    assert_eq!(join.is_cancelled(), cancelled);
    assert!(
        error
            .kind
            .source()
            .unwrap()
            .downcast_ref::<tokio::task::JoinError>()
            .is_some()
    );
    assert_eq!(
        error.to_string(),
        "rook owner operation failed: owner worker failed"
    );
}

#[tokio::test]
async fn cancelled_worker_join_is_typed_without_changing_mailbox_unavailable() {
    let task = tokio::spawn(std::future::pending::<()>());
    task.abort();
    let error = OwnerError::worker_join(task.await.unwrap_err());
    assert_worker_join(&error, false, true);
    assert!(matches!(
        OwnerError::new(OwnerErrorKind::Unavailable).kind,
        OwnerErrorKind::Unavailable
    ));
}

#[tokio::test]
async fn wake_worker_panic_keeps_join_cause_through_owner_shutdown() {
    let fixture = Fixture::new();
    fixture.transport.outcome.store(2, Ordering::SeqCst);
    let service = fixture.open().await;
    let observation = event("worker-panic", "body");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation).await.unwrap();
    // The wake callback publishes entry before its deliberate test panic.
    let mut fixture = fixture;
    fixture.next().await;
    tokio::time::timeout(
        Duration::from_secs(2),
        service.handle.wait_until_stopped_for_test(),
    )
    .await
    .unwrap();
    let error = service.shutdown().await.unwrap_err();
    assert_worker_join(&error, true, false);
    assert!(!error.to_string().contains("private-wake-worker-sentinel"));
}

struct ShutdownDropGuard {
    dropped: Arc<AtomicBool>,
    panic_on_drop: bool,
}
impl Drop for ShutdownDropGuard {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
        if self.panic_on_drop {
            panic!("private-shutdown-drop-sentinel");
        }
    }
}
struct ShutdownDropTransport {
    entered: Notify,
    dropped: Arc<AtomicBool>,
    calls: std::sync::atomic::AtomicUsize,
    panic_on_drop: bool,
}
impl WakeTransport for ShutdownDropTransport {
    fn deliver(
        &self,
        _request: WakeRequest,
    ) -> Pin<Box<dyn Future<Output = CallbackOutcome> + Send + '_>> {
        Box::pin(async move {
            let _guard = ShutdownDropGuard {
                dropped: self.dropped.clone(),
                panic_on_drop: self.panic_on_drop,
            };
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            std::future::pending::<CallbackOutcome>().await
        })
    }
}
async fn shutdown_drop_case(panic_on_drop: bool) {
    let mut fixture = Fixture::new();
    fixture.limits.attempts_per_binding = 1;
    let transport = Arc::new(ShutdownDropTransport {
        entered: Notify::new(),
        dropped: Arc::new(AtomicBool::new(false)),
        calls: std::sync::atomic::AtomicUsize::new(0),
        panic_on_drop,
    });
    let service = RookService::open(
        fixture.root.path().into(),
        fixture.id.clone(),
        fixture.limits.clone(),
        fixture.clock.clone(),
        fixture.access.clone(),
        transport.clone(),
    )
    .await
    .unwrap();
    let observation = event("shutdown-drop", "body");
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
    tokio::time::timeout(Duration::from_secs(2), transport.entered.notified())
        .await
        .unwrap();
    let before = retained(&fixture).await;
    assert!(matches!(
        before.events[0].links[0].wake,
        WakeState::Dispatching { .. }
    ));
    let result = service.shutdown().await;
    assert!(transport.dropped.load(Ordering::SeqCst));
    if panic_on_drop {
        let error = result.unwrap_err();
        assert_worker_join(&error, true, false);
        assert!(!error.to_string().contains("private-shutdown-drop-sentinel"));
    } else {
        result.unwrap();
    }
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    let after = retained(&fixture).await;
    assert!(matches!(
        after.events[0].links[0].wake,
        WakeState::Dispatching { .. }
    ));
    assert_eq!(after.events[0].observation.key(), observation.key());
    // Reopening the real owner proves the disk worker released the private lock.
    let reopened = fixture.open().await;
    reopened.handle.tick().await.unwrap();
    let status = reopened.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(
        status.events[0].disposition,
        EventDisposition::Parked(ParkReason::DeliveryAttemptsExhausted)
    );
    assert_eq!(status.events[0].attempts[0].1, 1);
    assert_eq!(
        status.events[0].attempts[0].2,
        Some(DeliveryFailure::Restarted)
    );
    assert!(fixture.sent.try_recv().is_err());
    assert_eq!(transport.calls.load(Ordering::SeqCst), 1);
    reopened.shutdown().await.unwrap();
}
#[tokio::test]
async fn shutdown_wake_drop_panic_retains_typed_join_and_uncertain_state() {
    shutdown_drop_case(true).await;
}
#[tokio::test]
async fn shutdown_expected_wake_cancellation_drains_without_error() {
    shutdown_drop_case(false).await;
}
