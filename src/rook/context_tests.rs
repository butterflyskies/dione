use super::*;
use crate::rook::{
    ContextCapabilities, ContextDirection, ContextError, ContextErrorKind,
    ContextObservation as ReadObservation, ContextPage, ContextProvider, ContextQuery,
    ContextRequest, ContextResponse, ContextService, ContextTools, MessageKey, PageLimit,
};

fn assert_owner_denied(error: ContextError) {
    assert_eq!(error.kind, ContextErrorKind::Owner);
    let source = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<OwnerError>()
        .unwrap();
    assert!(matches!(source.kind, OwnerErrorKind::Denied));
}

struct Reader {
    calls: AtomicU8,
    blocked: AtomicBool,
    entered: Notify,
    release: Notify,
    response: ContextResponse,
    fail: AtomicBool,
    capabilities: ContextCapabilities,
}

impl ContextProvider for Reader {
    fn capabilities(&self) -> ContextCapabilities {
        self.capabilities
    }

    fn read<'a>(
        &'a self,
        _: &'a ContextQuery,
    ) -> Pin<Box<dyn Future<Output = Result<ContextResponse, ContextError>> + Send + 'a>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            if self.blocked.load(Ordering::SeqCst) {
                self.release.notified().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                Err(ContextError::new(ContextErrorKind::NotFound))
            } else {
                Ok(self.response.clone())
            }
        })
    }
}

fn message(id: &str, room: &str, slot: &str) -> ContextObservation {
    let source = ModuleInstanceId::new(
        ProviderId::new("test").unwrap(),
        ModuleSlotId::new(slot).unwrap(),
    );
    ContextObservation::new(
        MessageRef::new(
            ConversationRef::new(source.clone(), ConversationId::opaque(room).unwrap()),
            MessageId::opaque(id).unwrap(),
        ),
        SenderObservation::new(
            ActorRef::new(source, ActorId::opaque("author").unwrap()),
            "author".into(),
            Availability::Known(false),
            Availability::Unavailable(UnavailableReason::NotReported),
        ),
        "authoritative read".into(),
        "2026-10-14T11:59:00Z".parse().unwrap(),
        ContextMetadata::new(
            Availability::Unavailable(UnavailableReason::NotReported),
            Availability::Known(None),
            Availability::Unavailable(UnavailableReason::NotReported),
            Availability::Known(Vec::new()),
            ObservationExtent::new(ObservationOrigin::History),
        ),
    )
}

fn reader(response: ContextResponse) -> Arc<Reader> {
    Arc::new(Reader {
        calls: AtomicU8::new(0),
        blocked: AtomicBool::new(false),
        entered: Notify::new(),
        release: Notify::new(),
        response,
        fail: AtomicBool::new(false),
        capabilities: ContextCapabilities {
            exact: true,
            history: true,
            max_page_size: 100,
        },
    })
}

async fn configured(fixture: &Fixture) -> (RookService, RetainedObservation) {
    let service = fixture.open().await;
    let observation = event("original", "preview");
    service
        .handle
        .configure(
            operator(),
            vec![subscription(&observation, fixture.clock.now())],
        )
        .await
        .unwrap();
    service.handle.admit(observation.clone()).await.unwrap();
    (service, observation)
}

fn exact(event: &RetainedObservation) -> ContextRequest {
    ContextRequest {
        event: event.key().clone(),
        query: ContextQuery::Exact(event.key().message().clone()),
    }
}

#[tokio::test]
async fn context_principal_mismatch_never_calls_provider() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let other = AuthenticatedRecipient::from_authenticated_identity(RecipientId::new());
    assert_owner_denied(service.read(other, exact(&event)).await.unwrap_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_wrong_room_or_module_never_calls_provider() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    for (room, slot) in [("excluded", "primary"), ("room", "other")] {
        let target = message("original", room, slot);
        let request = ContextRequest {
            event: event.key().clone(),
            query: ContextQuery::Exact(MessageKey::from(target.reference())),
        };
        assert!(service.read(fixture.recipient(), request).await.is_err());
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_current_access_failure_keeps_subscription_enabled() {
    for decision in [1, 2] {
        let fixture = Fixture::new();
        let (owner, event) = configured(&fixture).await;
        let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
            "original", "room", "primary",
        ))));
        let service = ContextService::new(owner.handle.clone(), provider.clone());
        fixture.access.decision.store(decision, Ordering::SeqCst);
        let failure = service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap_err();
        assert!(matches!(
            (decision, failure.kind),
            (1, ContextErrorKind::AccessDenied) | (2, ContextErrorKind::AccessUnavailable)
        ));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            owner
                .handle
                .status(fixture.recipient())
                .await
                .unwrap()
                .enabled_subscriptions,
            1
        );
        owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn context_revocation_during_provider_read_prevents_disclosure() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    provider.blocked.store(true, Ordering::SeqCst);
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let principal = fixture.recipient();
    let read = tokio::spawn(async move { service.read(principal, exact(&event)).await });
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .unwrap();
    owner
        .handle
        .configure(operator(), Vec::new())
        .await
        .unwrap();
    provider.release.notify_one();
    assert!(read.await.unwrap().is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_read_without_claim_does_not_settle_or_disable() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    assert!(matches!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap(),
        ContextResponse::Exact(_)
    ));
    let status = owner.handle.status(fixture.recipient()).await.unwrap();
    assert_eq!(status.enabled_subscriptions, 1);
    assert!(matches!(
        status.events[0].disposition,
        EventDisposition::Waiting
    ));
    assert!(retained(&fixture).await.claims.is_empty());
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_provider_wrong_exact_identity_is_not_disclosed() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "wrong", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    assert!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .is_err()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        owner
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_provider_failure_is_visible_without_disabling_subscription() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    provider.fail.store(true, Ordering::SeqCst);
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    assert!(matches!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap_err()
            .kind,
        ContextErrorKind::NotFound
    ));
    assert_eq!(
        owner
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    assert!(retained(&fixture).await.claims.is_empty());
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_current_access_revoked_during_provider_read_prevents_disclosure() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    provider.blocked.store(true, Ordering::SeqCst);
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let principal = fixture.recipient();
    let entered = provider.entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut read = tokio::spawn(async move { service.read(principal, exact(&event)).await });
    enum Entry {
        Entered,
        Completed(Result<Result<ContextResponse, ContextError>, tokio::task::JoinError>),
        Deadline,
    }
    let entry = tokio::select! {
        biased;
        _ = &mut entered => Entry::Entered,
        outcome = &mut read => Entry::Completed(outcome),
        _ = tokio::time::sleep_until(deadline) => Entry::Deadline,
    };
    if !matches!(entry, Entry::Entered) {
        let timed_out = matches!(entry, Entry::Deadline);
        let outcome = match entry {
            Entry::Completed(outcome) => outcome,
            Entry::Deadline => {
                read.abort();
                read.await
            }
            Entry::Entered => unreachable!(),
        };
        let calls = provider.calls.load(Ordering::SeqCst);
        provider.release.notify_one();
        let shutdown = owner.shutdown().await;
        assert!(
            shutdown.is_ok(),
            "owner must drain after failed entry handshake"
        );
        match outcome {
            Ok(Err(error)) => panic!(
                "provider entry failed: deadline={timed_out}, calls={calls}, kind={}",
                error.kind
            ),
            Ok(Ok(_)) => panic!(
                "read completed before blocked provider entry: deadline={timed_out}, calls={calls}"
            ),
            Err(error) => panic!(
                "provider entry task ended: deadline={timed_out}, calls={calls}, panic={}, cancelled={}",
                error.is_panic(),
                error.is_cancelled()
            ),
        }
    }
    fixture.access.decision.store(1, Ordering::SeqCst);
    provider.release.notify_one();
    assert!(matches!(
        read.await.unwrap().unwrap_err().kind,
        ContextErrorKind::AccessDenied
    ));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_history_cross_room_anchor_is_rejected_before_provider() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let foreign = MessageKey::from(message("anchor", "excluded", "primary").reference());
    for direction in [
        ContextDirection::Before(foreign.clone()),
        ContextDirection::After(foreign),
    ] {
        let request = ContextRequest {
            event: event.key().clone(),
            query: ContextQuery::History {
                room: event.conversation().clone(),
                direction,
                limit: PageLimit::new(5).unwrap(),
            },
        };
        assert!(matches!(
            service
                .read(fixture.recipient(), request)
                .await
                .unwrap_err()
                .kind,
            ContextErrorKind::InvalidReference
        ));
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_history_response_cannot_change_requested_direction() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let wrong_query = ContextQuery::History {
        room: event.conversation().clone(),
        direction: ContextDirection::Recent,
        limit: PageLimit::new(5).unwrap(),
    };
    let provider = reader(ContextResponse::history(
        ContextPage::new(wrong_query, Vec::new(), false).unwrap(),
    ));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let request = ContextRequest {
        event: event.key().clone(),
        query: ContextQuery::History {
            room: event.conversation().clone(),
            direction: ContextDirection::Before(event.key().message().clone()),
            limit: PageLimit::new(5).unwrap(),
        },
    };
    assert!(matches!(
        service
            .read(fixture.recipient(), request)
            .await
            .unwrap_err()
            .kind,
        ContextErrorKind::InvalidResponse
    ));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_unsupported_operation_is_typed_and_does_not_call_provider() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let mut provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    Arc::get_mut(&mut provider).unwrap().capabilities.exact = false;
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    assert!(matches!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap_err()
            .kind,
        ContextErrorKind::Unsupported
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_tool_reads_original_without_fabricating_occurrence() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let tools = ContextTools::new(
        fixture.recipient(),
        ContextService::new(owner.handle.clone(), provider),
    );
    let output = tools
        .call(
            "rook_get_message",
            serde_json::json!({
                "event": event.key(), "target": event.key().message(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(output["kind"], "exact");
    assert!(output["result"].get("key").is_none());
    assert!(output["result"].get("occurrence").is_none());
    assert_eq!(output["result"]["context"]["content"], "authoritative read");
    assert_eq!(output["result"]["context"]["extent"]["origin"], "history");
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_tool_rejects_ambiguous_direction_and_caller_authority() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let tools = ContextTools::new(
        fixture.recipient(),
        ContextService::new(owner.handle.clone(), provider.clone()),
    );
    assert!(tools.call("rook_get_context", serde_json::json!({
        "event": event.key(), "room": event.conversation(), "limit": 5,
        "direction": {"kind": "before", "anchor": event.key().message(), "after": event.key().message()},
    })).await.is_err());
    assert!(tools.call("rook_get_message", serde_json::json!({
        "event": event.key(), "target": event.key().message(), "recipient": RecipientId::new(),
    })).await.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_history_tool_preserves_anchor_and_observed_bounds_separately() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let query = ContextQuery::History {
        room: event.conversation().clone(),
        direction: ContextDirection::Before(event.key().message().clone()),
        limit: PageLimit::new(5).unwrap(),
    };
    let first = ReadObservation::capture(&message("older", "room", "primary"));
    let last = ReadObservation::capture(&message("newer", "room", "primary"));
    let page = ContextPage::new(query, vec![first.clone(), last.clone()], true).unwrap();
    let provider = reader(ContextResponse::history(page));
    let tools = ContextTools::new(
        fixture.recipient(),
        ContextService::new(owner.handle.clone(), provider.clone()),
    );
    let output = tools
        .call(
            "rook_get_context",
            serde_json::json!({
                "event": event.key(), "room": event.conversation(), "limit": 5,
                "direction": {"kind": "before", "anchor": event.key().message()},
            }),
        )
        .await
        .unwrap();
    assert_eq!(output["kind"], "history");
    assert_eq!(
        output["result"]["query"]["query"]["direction"]["anchor"],
        serde_json::to_value(event.key().message()).unwrap()
    );
    assert_eq!(
        output["result"]["oldest"],
        serde_json::to_value(first.reference()).unwrap()
    );
    assert_eq!(
        output["result"]["newest"],
        serde_json::to_value(last.reference()).unwrap()
    );
    assert_eq!(output["result"]["has_more_hint"], true);
    assert_eq!(output["result"]["messages"].as_array().unwrap().len(), 2);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_oversized_provider_response_is_refused_without_disabling() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let original = ReadObservation::capture(&message("original", "room", "primary"));
    let mut wire = serde_json::to_value(original).unwrap();
    wire["context"]["content"] = serde_json::Value::String("x".repeat(4 * 1024 * 1024));
    let oversized = serde_json::from_value::<ReadObservation>(wire).unwrap();
    let provider = reader(ContextResponse::exact(oversized));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    assert!(matches!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap_err()
            .kind,
        ContextErrorKind::ResponseTooLarge
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        owner
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    assert!(retained(&fixture).await.claims.is_empty());
    owner.shutdown().await.unwrap();
}

#[test]
fn context_page_rejects_invalid_limits_and_duplicate_message_identities() {
    for invalid in [0_u8, 101] {
        assert!(matches!(
            PageLimit::new(invalid).unwrap_err().kind,
            ContextErrorKind::InvalidLimit
        ));
        assert!(serde_json::from_value::<PageLimit>(serde_json::json!(invalid)).is_err());
    }
    let observation = event("original", "preview");
    let query = ContextQuery::History {
        room: observation.conversation().clone(),
        direction: ContextDirection::Recent,
        limit: PageLimit::new(5).unwrap(),
    };
    let duplicate = ReadObservation::capture(&message("original", "room", "primary"));
    assert!(matches!(
        ContextPage::new(query, vec![duplicate.clone(), duplicate], false)
            .unwrap_err()
            .kind,
        ContextErrorKind::InvalidResponse
    ));
}

#[tokio::test]
async fn context_policy_revision_change_with_retained_eligibility_fences_old_read() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let mut original_subscription = retained(&fixture).await.subscriptions[0].clone();
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    provider.blocked.store(true, Ordering::SeqCst);
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let pending_service = service.clone();
    let principal = fixture.recipient();
    let request = exact(&event);
    let pending = tokio::spawn(async move { pending_service.read(principal, request).await });
    tokio::time::timeout(Duration::from_secs(2), provider.entered.notified())
        .await
        .unwrap();
    let before = owner
        .handle
        .status(fixture.recipient())
        .await
        .unwrap()
        .authorization_revision;
    original_subscription.rooms.insert(
        MessageKey::from(message("other", "extra-room", "primary").reference())
            .conversation()
            .clone(),
    );
    owner
        .handle
        .configure(operator(), vec![original_subscription])
        .await
        .unwrap();
    let after = owner
        .handle
        .status(fixture.recipient())
        .await
        .unwrap()
        .authorization_revision;
    assert_ne!(before, after);
    provider.blocked.store(false, Ordering::SeqCst);
    provider.release.notify_one();
    assert_owner_denied(pending.await.unwrap().unwrap_err());
    assert!(matches!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .unwrap(),
        ContextResponse::Exact(_)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_selective_sender_admission_does_not_filter_other_history_speakers() {
    let fixture = Fixture::new();
    let owner = fixture.open().await;
    let event = event("original", "preview");
    let mut selected = subscription(&event, fixture.clock.now());
    selected.senders = SenderFilter::Only(BTreeSet::from([event.sender().clone()]));
    owner
        .handle
        .configure(operator(), vec![selected])
        .await
        .unwrap();
    owner.handle.admit(event.clone()).await.unwrap();
    let original = ReadObservation::capture(&message("another-message", "room", "primary"));
    let mut wire = serde_json::to_value(original).unwrap();
    wire["context"]["sender"]["reference"]["local"]["value"] = "another-speaker".into();
    let other: ReadObservation = serde_json::from_value(wire).unwrap();
    assert_ne!(other.sender(), event.sender());
    let query = ContextQuery::History {
        room: event.conversation().clone(),
        direction: ContextDirection::Recent,
        limit: PageLimit::new(5).unwrap(),
    };
    let provider = reader(ContextResponse::history(
        ContextPage::new(query.clone(), vec![other.clone()], false).unwrap(),
    ));
    let service = ContextService::new(owner.handle.clone(), provider);
    let response = service
        .read(
            fixture.recipient(),
            ContextRequest {
                event: event.key().clone(),
                query,
            },
        )
        .await
        .unwrap();
    let ContextResponse::History(page) = response else {
        panic!("expected history response")
    };
    assert_eq!(page.messages(), &[other]);
    assert_eq!(
        owner
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    owner.shutdown().await.unwrap();
}

async fn wait_for_reader_calls(provider: &Reader, expected: u8) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let entered = provider.entered.notified();
            if provider.calls.load(Ordering::SeqCst) >= expected {
                return;
            }
            entered.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn context_cancellation_releases_shared_read_quota_for_waiting_request() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    provider.blocked.store(true, Ordering::SeqCst);
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let mut pending = Vec::new();
    for _ in 0..4 {
        let service = service.clone();
        let principal = fixture.recipient();
        let request = exact(&event);
        pending.push(tokio::spawn(async move {
            service.read(principal, request).await
        }));
    }
    wait_for_reader_calls(&provider, 4).await;
    let fifth = service.read(fixture.recipient(), exact(&event));
    tokio::pin!(fifth);
    assert!(futures_util::poll!(fifth.as_mut()).is_pending());
    owner.handle.status(fixture.recipient()).await.unwrap();
    assert!(futures_util::poll!(fifth.as_mut()).is_pending());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 4);

    let canceled = pending.remove(0);
    canceled.abort();
    assert!(canceled.await.unwrap_err().is_cancelled());
    assert!(futures_util::poll!(fifth.as_mut()).is_pending());
    owner.handle.status(fixture.recipient()).await.unwrap();
    assert!(futures_util::poll!(fifth.as_mut()).is_pending());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 5);
    for task in pending {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    provider.blocked.store(false, Ordering::SeqCst);
    provider.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), fifth)
            .await
            .unwrap()
            .is_ok()
    );
    assert!(
        service
            .read(fixture.recipient(), exact(&event))
            .await
            .is_ok()
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 6);
    assert_eq!(
        owner
            .handle
            .status(fixture.recipient())
            .await
            .unwrap()
            .enabled_subscriptions,
        1
    );
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_tool_input_decode_retains_nested_path_and_sanitized_display() {
    let fixture = Fixture::new();
    let (owner, event) = configured(&fixture).await;
    let provider = reader(ContextResponse::exact(ReadObservation::capture(&message(
        "original", "room", "primary",
    ))));
    let service = ContextService::new(owner.handle.clone(), provider.clone());
    let tools = ContextTools::new(fixture.recipient(), service);
    let mut arguments = serde_json::json!({"event": event.key(), "target": event.key().message()});
    arguments["target"]["conversation"]["source"]["provider"] = serde_json::json!(123);
    let error = tools.call("rook_get_message", arguments).await.unwrap_err();
    assert_eq!(error.kind, ContextErrorKind::InvalidArguments);
    let cause = std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<serde_path_to_error::Error<serde_json::Error>>()
        .expect("nested typed decoding cause must survive");
    assert_eq!(
        cause.path().to_string(),
        "target.conversation.source.provider"
    );
    assert!(cause.inner().is_data());
    assert!(!error.to_string().contains("123"));
    let history_arguments = serde_json::json!({
        "event": event.key(), "room": event.key().message().conversation(),
        "direction": {"kind": "recent"}, "limit": "private-limit-sentinel"
    });
    let history_error = tools
        .call("rook_get_context", history_arguments)
        .await
        .unwrap_err();
    assert_eq!(history_error.kind, ContextErrorKind::InvalidArguments);
    let cause = std::error::Error::source(&history_error)
        .unwrap()
        .downcast_ref::<serde_path_to_error::Error<serde_json::Error>>()
        .unwrap();
    assert_eq!(cause.path().to_string(), "limit");
    assert!(cause.inner().is_data());
    assert!(!history_error.to_string().contains("private-limit-sentinel"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    owner.shutdown().await.unwrap();
}
