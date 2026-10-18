//! Lifecycle regressions exercise the retained command owner and persisted inbox.
use super::*;
use crate::{
    durable::{PersistFailure, QueueCodec},
    teams::{ActivityId, BotId, ConversationId, ReplyHandle, TenantId},
    timestamp::Timestamp,
};
use camino::Utf8PathBuf;
use chrono::Utc;
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    time::timeout,
};

fn event(activity: &str) -> TeamsResidentEvent {
    let tenant_id: TenantId = "tenant".parse().unwrap();
    let bot_id: BotId = "bot".parse().unwrap();
    let conversation_id: ConversationId = "19:conversation@thread.v2".parse().unwrap();
    let message_id: ActivityId = activity.parse().unwrap();
    TeamsResidentEvent {
        provider_key: ProviderEventKey::teams(
            tenant_id.clone(),
            bot_id.clone(),
            conversation_id.clone(),
            message_id.clone(),
        ),
        content: "external conversation".into(),
        message_id,
        conversation_id,
        channel_id: "msteams".into(),
        user_id: "29:participant".parse().unwrap(),
        tenant_id,
        bot_id,
        reply_handle: ReplyHandle::generate(),
        timestamp: Timestamp::now(),
    }
}
fn path(directory: &TempDir) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap()
}
async fn ready(control: &OmpControl, session: &str) -> OmpBinding {
    control.initialized().await.unwrap();
    control
        .bind(SavedSessionId::parse(session).unwrap())
        .await
        .unwrap()
}
async fn lease(queue: &Inbox) -> DeliveryToken {
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(token) = queue
                .test_snapshot()
                .await
                .entries
                .iter()
                .find_map(|entry| entry.lease.as_ref().map(|lease| lease.token.clone()))
            {
                return token;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
async fn paused_admission(
    admission: OmpAdmission,
) -> (
    JoinHandle<Result<AdmissionOutcome, OmpSessionError>>,
    oneshot::Sender<()>,
) {
    let (controls, received) = oneshot::channel();
    let task = tokio::spawn(async move {
        request(&admission.sender, |reply| {
            Command::PausedAdmit(
                Box::new(event("one")),
                reply,
                Pause {
                    lose_response: false,
                    controls,
                },
            )
        })
        .await
    });
    let (reached, release) = received.await.unwrap();
    reached.await.unwrap();
    (task, release)
}
async fn paused_bind(
    control: OmpControl,
    lose_response: bool,
) -> (
    JoinHandle<Result<OmpBinding, OmpSessionError>>,
    oneshot::Sender<()>,
) {
    let (controls, received) = oneshot::channel();
    let task = tokio::spawn(async move {
        request(&control.sender, |reply| {
            Command::PausedBind(
                SavedSessionId::parse("saved-one").unwrap(),
                reply,
                Pause {
                    lose_response,
                    controls,
                },
            )
        })
        .await
    });
    let (reached, release) = received.await.unwrap();
    reached.await.unwrap();
    (task, release)
}

#[tokio::test]
async fn duplicate_survives_disconnect_and_restart_without_admitting_fresh_events() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap_err().kind,
        OmpSessionErrorKind::Unavailable
    ));
    ready(&control, "saved-one").await;
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    control.disconnect(None).await.unwrap();
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Duplicate
    ));
    assert!(matches!(
        admission.enqueue(event("two")).await.unwrap_err().kind,
        OmpSessionErrorKind::Unavailable
    ));
    running.shutdown().await.unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Duplicate
    ));
    assert!(matches!(
        admission.enqueue(event("two")).await.unwrap_err().kind,
        OmpSessionErrorKind::Unavailable
    ));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn continuously_ready_commands_do_not_starve_delivery_or_shutdown() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let commands = session.sender.clone();
    let (writer, reader) = tokio::io::duplex(8192);
    let running = session.start(ProtocolOutput::new(writer), CancellationToken::new());
    ready(&control, "saved-one").await;
    let (admitting, release) = paused_admission(admission).await;
    let handled = Arc::new(AtomicUsize::new(0));
    assert!(
        commands
            .send(Command::KeepBusy(commands.clone(), handled.clone()))
            .await
            .is_ok()
    );
    release.send(()).unwrap();
    assert!(matches!(
        admitting.await.unwrap().unwrap(),
        AdmissionOutcome::Committed
    ));
    timeout(Duration::from_secs(5), async {
        while handled.load(Ordering::Relaxed) <= COMMAND_CAPACITY {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut lines = BufReader::new(reader).lines();
    let received = timeout(Duration::from_secs(5), lines.next_line()).await;
    let stopped = timeout(Duration::from_secs(5), running.shutdown()).await;
    stopped
        .expect("shutdown must close and drain a continuously ready mailbox")
        .unwrap();
    let line = received
        .expect("a due delivery must run ahead of queued commands")
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["params"]["meta"]["message_id"], "one");
    assert_eq!(value["params"]["meta"]["mcp_session_id"], "saved-one");
    assert!(handled.load(Ordering::Relaxed) > COMMAND_CAPACITY);
}

#[tokio::test]
async fn cancelled_admission_finishes_before_rebind_and_keeps_original_endpoint() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    ready(&control, "saved-one").await;
    let (request, release) = paused_admission(admission).await;
    let committed = OmpCodec
        .decode(
            &tokio::fs::read(path(&directory).join("omp-inbox.json"))
                .await
                .unwrap(),
        )
        .unwrap();
    assert_eq!(committed.entries.len(), 1);
    assert_eq!(
        committed.entries[0].endpoint.as_ref().unwrap().as_str(),
        "saved-one"
    );
    request.abort();
    let changing = tokio::spawn(async move {
        control
            .bind(SavedSessionId::parse("saved-two").unwrap())
            .await
    });
    tokio::task::yield_now().await;
    assert!(!changing.is_finished());
    release.send(()).unwrap();
    changing.await.unwrap().unwrap();
    let snapshot = queue.test_snapshot().await;
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(
        snapshot.entries[0].endpoint.as_ref().unwrap().as_str(),
        "saved-one"
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_bind_publishes_authoritative_state_before_following_admission() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    control.initialized().await.unwrap();
    let (binding, release) = paused_bind(control, false).await;
    binding.abort();
    release.send(()).unwrap();
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    assert_eq!(
        queue.test_snapshot().await.entries[0]
            .endpoint
            .as_ref()
            .unwrap()
            .as_str(),
        "saved-one"
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn rebind_rejects_old_generation_and_old_token_even_with_new_generation() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let first = ready(&control, "saved-one").await;
    admission.enqueue(event("one")).await.unwrap();
    let old_token = lease(&queue).await;
    let second = control.bind(first.session_id.clone()).await.unwrap();
    assert_ne!(first.binding_id, second.binding_id);
    assert!(matches!(
        control
            .ack(first.binding_id.clone(), old_token.clone())
            .await
            .unwrap_err()
            .kind,
        OmpSessionErrorKind::StaleBinding
    ));
    assert!(
        matches!(control.ack(second.binding_id.clone(), old_token).await.unwrap_err().kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::UnknownDeliveryToken))
    );
    assert!(matches!(
        control
            .disconnect(Some(first.binding_id))
            .await
            .unwrap_err()
            .kind,
        OmpSessionErrorKind::StaleBinding
    ));
    let current = lease(&queue).await;
    assert!(control.ack(second.binding_id, current).await.unwrap());
    assert!(queue.test_snapshot().await.entries.is_empty());
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn another_session_cannot_receive_old_backlog_but_global_duplicate_is_retained() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let first = ready(&control, "saved-one").await;
    admission.enqueue(event("one")).await.unwrap();
    control
        .bind(SavedSessionId::parse("saved-two").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Duplicate
    ));
    let before = queue.test_snapshot().await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let after = queue.test_snapshot().await;
    assert_eq!(
        after.entries[0].endpoint.as_ref().unwrap(),
        first.session_id.endpoint()
    );
    assert_eq!(
        after.entries[0].lease.as_ref().map(|lease| &lease.token),
        before.entries[0].lease.as_ref().map(|lease| &lease.token)
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncertain_rebind_revokes_admission_until_explicit_rebind() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    ready(&control, "saved-one").await;
    let error = request(&control.sender, |reply| {
        Command::FaultedBind(
            SavedSessionId::parse("saved-two").unwrap(),
            PersistFailure::AfterRename,
            reply,
        )
    })
    .await
    .unwrap_err();
    assert!(
        matches!(&error.kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::DurabilityUncertain(_))),
        "rebind must fail after its own rename: {error:?}"
    );
    let persisted = OmpCodec
        .decode(
            &tokio::fs::read(path(&directory).join("omp-inbox.json"))
                .await
                .unwrap(),
        )
        .unwrap();
    assert_eq!(persisted.endpoint.unwrap().as_str(), "saved-two");
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap_err().kind,
        OmpSessionErrorKind::Unavailable
    ));
    control
        .bind(SavedSessionId::parse("saved-two").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn pump_uncertainty_makes_later_rebind_report_prior_sync() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    ready(&control, "saved-one").await;
    let baseline = queue.test_directory_sync_attempts().await;
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    let pump = timeout(Duration::from_secs(5), async {
        loop {
            let (_, pending, attempts) = queue.test_state().await;
            if pending && attempts > baseline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if let Err(error) = pump {
        queue.test_set_persist_failure(None).await;
        running.shutdown().await.unwrap();
        panic!("pump did not leave a pending directory sync: {error}");
    }
    let binding = control
        .bind(SavedSessionId::parse("saved-two").unwrap())
        .await;
    queue.test_set_persist_failure(None).await;
    let persisted = tokio::fs::read(path(&directory).join("omp-inbox.json")).await;
    let unavailable = admission.enqueue(event("one")).await;
    let recovery = control
        .bind(SavedSessionId::parse("saved-two").unwrap())
        .await;
    let accepted = admission.enqueue(event("one")).await;
    running.shutdown().await.unwrap();

    let error = binding.unwrap_err();
    assert!(
        matches!(&error.kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::PriorSync(_))),
        "earlier pump uncertainty must stop rebind at prior sync: {error:?}"
    );
    let persisted = OmpCodec.decode(&persisted.unwrap()).unwrap();
    assert_eq!(persisted.endpoint.unwrap().as_str(), "saved-one");
    let error = unavailable.unwrap_err();
    assert!(
        matches!(error.kind, OmpSessionErrorKind::Unavailable),
        "pump uncertainty must revoke fresh admission: {error:?}"
    );
    recovery.unwrap();
    assert!(matches!(accepted.unwrap(), AdmissionOutcome::Committed));
}

#[tokio::test]
async fn shutdown_drains_accepted_admission_before_releasing_lock() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    ready(&control, "saved-one").await;
    let (request, release) = paused_admission(admission).await;
    let shutdown = tokio::spawn(running.shutdown());
    tokio::task::yield_now().await;
    assert!(!shutdown.is_finished());
    release.send(()).unwrap();
    assert!(matches!(
        request.await.unwrap().unwrap(),
        AdmissionOutcome::Committed
    ));
    shutdown.await.unwrap().unwrap();
    assert_eq!(queue.test_snapshot().await.entries.len(), 1);
    drop(queue);
    let reopened = OmpSession::open(&path(&directory)).await.unwrap();
    assert_eq!(reopened.queue.test_snapshot().await.entries.len(), 1);
}

#[tokio::test]
async fn lost_durable_bind_response_revokes_binding_and_explicit_bind_recovers() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    control.initialized().await.unwrap();
    let (binding, release) = paused_bind(control.clone(), true).await;
    release.send(()).unwrap();
    assert!(
        matches!(binding.await.unwrap().unwrap_err().kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::CompletionUnknown(_)))
    );
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap_err().kind,
        OmpSessionErrorKind::Unavailable
    ));
    control
        .bind(SavedSessionId::parse("saved-one").unwrap())
        .await
        .unwrap();
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_precommit_rebind_keeps_previous_session_binding() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    ready(&control, "saved-one").await;
    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    assert!(
        matches!(control.bind(SavedSessionId::parse("saved-two").unwrap()).await.unwrap_err().kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::Io(_)))
    );
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        admission.enqueue(event("one")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    assert_eq!(
        queue.test_snapshot().await.entries[0]
            .endpoint
            .as_ref()
            .unwrap()
            .as_str(),
        "saved-one"
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_lease_is_rejected_and_resident_renews_after_long_idle() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let admission = session.admission();
    let control = session.control();
    let queue = session.queue.clone();
    let now = Utc::now();
    queue.test_set_now(now);
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let binding = ready(&control, "saved-one").await;
    admission.enqueue(event("one")).await.unwrap();
    let token = lease(&queue).await;
    queue.test_set_now(now + chrono::Duration::days(2));
    assert!(
        matches!(control.ack(binding.binding_id, token).await.unwrap_err().kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::UnknownDeliveryToken))
    );
    assert_eq!(queue.test_snapshot().await.entries.len(), 1);
    assert!(matches!(
        admission.enqueue(event("two")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    running.shutdown().await.unwrap();
}

#[test]
fn notification_keeps_native_ids_and_large_event_id_as_decimal_string() {
    let payload = OmpPayload::from_event(event("activity/with-native-spelling")).unwrap();
    let frame = Frame {
        binding: OmpBinding {
            session_id: SavedSessionId::parse("saved-one").unwrap(),
            binding_id: BindingId::generate(),
        },
        event: LeasedEvent {
            event_id: crate::durable::EventId::new(u64::MAX),
            delivery_token: DeliveryToken::parse("lease").unwrap(),
            lease_expires_at: Utc::now(),
            consumer_id: crate::durable::ConsumerId::parse("omp-1").unwrap(),
            payload,
        },
    };
    let wire = notification(frame);
    assert_eq!(wire["params"]["meta"]["event_id"], "18446744073709551615");
    assert_eq!(
        wire["params"]["meta"]["message_id"],
        "activity/with-native-spelling"
    );
    assert_eq!(
        wire["params"]["meta"]["conversation_id"],
        "19:conversation@thread.v2"
    );
}

#[test]
fn codec_rejects_public_records_and_mismatched_native_replay_identity() {
    let mut snapshot = crate::durable::QueueSnapshot::default();
    let payload = OmpPayload::from_event(event("one")).unwrap();
    snapshot.entries.push_back(crate::durable::state::Entry {
        id: crate::durable::EventId::new(0),
        replay_keys: vec![payload.key()],
        payload,
        consumer_id: Some(crate::durable::ConsumerId::parse("omp-1").unwrap()),
        endpoint: Some(crate::durable::EndpointId::new("saved-one").unwrap()),
        visibility: crate::durable::EntryVisibility::Public,
        deferrable: false,
        deferred_until: None,
        lease: None,
    });
    assert!(OmpCodec.encode(&snapshot).is_err());
    snapshot.entries[0].visibility = crate::durable::EntryVisibility::ResidentOnly;
    let valid = OmpCodec.encode(&snapshot).unwrap();
    assert_eq!(OmpCodec.decode(&valid).unwrap().entries.len(), 1);
    snapshot.entries[0].replay_keys = vec![event("different").provider_key];
    assert!(OmpCodec.encode(&snapshot).is_err());
}

#[tokio::test]
async fn fenced_recovery_survives_actual_renewal_uncertainty_and_retries_its_receipt() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let queue = session.queue.clone();
    let admission = session.admission();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let first = ready(&control, "saved-one").await;
    let error = request(&control.sender, |reply| {
        Command::FaultedRenew(PersistFailure::AfterRename, reply)
    })
    .await
    .unwrap_err();
    assert!(
        matches!(&error.kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::DurabilityUncertain(_)))
    );
    assert_eq!(
        control
            .check(first.session_id.clone(), first.binding_id.clone())
            .await
            .unwrap(),
        BindingStatus::Inactive
    );
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(
        control
            .recover(first.session_id.clone(), first.binding_id.clone())
            .await
            .is_err()
    );
    assert!(admission.enqueue(event("blocked")).await.is_err());
    queue.test_set_persist_failure(None).await;
    let BindingRecovery::Active(second) = control
        .recover(first.session_id.clone(), first.binding_id.clone())
        .await
        .unwrap()
    else {
        panic!("recovery must confirm a new generation");
    };
    assert_ne!(first.binding_id, second.binding_id);
    assert_eq!(
        control
            .recover(first.session_id.clone(), first.binding_id.clone())
            .await
            .unwrap(),
        BindingRecovery::Active(second.clone())
    );
    assert!(matches!(
        admission.enqueue(event("recovered")).await.unwrap(),
        AdmissionOutcome::Committed
    ));
    let token = lease(&queue).await;
    assert!(matches!(
        control
            .ack(first.binding_id.clone(), token.clone())
            .await
            .unwrap_err()
            .kind,
        OmpSessionErrorKind::StaleBinding
    ));
    assert!(control.ack(second.binding_id.clone(), token).await.unwrap());
    request(&control.sender, |reply| {
        Command::FaultedRenew(PersistFailure::AfterRename, reply)
    })
    .await
    .unwrap_err();
    assert_eq!(
        control
            .recover(first.session_id.clone(), first.binding_id)
            .await
            .unwrap(),
        BindingRecovery::Inactive(second.clone())
    );
    let BindingRecovery::Active(third) = control
        .recover(second.session_id, second.binding_id.clone())
        .await
        .unwrap()
    else {
        panic!("owned inactive fence must recover");
    };
    assert_ne!(third.binding_id, second.binding_id);
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn fenced_recovery_refuses_newer_authority_and_explicit_disconnect() {
    for next in [Some("saved-two"), Some("saved-one"), None] {
        let directory = TempDir::new().unwrap();
        let session = OmpSession::open(&path(&directory)).await.unwrap();
        let control = session.control();
        let running = session.start(
            ProtocolOutput::new(tokio::io::sink()),
            CancellationToken::new(),
        );
        let first = ready(&control, "saved-one").await;
        request(&control.sender, |reply| {
            Command::FaultedRenew(PersistFailure::AfterRename, reply)
        })
        .await
        .unwrap_err();
        assert_eq!(
            control
                .check(first.session_id.clone(), first.binding_id.clone())
                .await
                .unwrap(),
            BindingStatus::Inactive
        );
        // This lifecycle action occurs between CheckInactive(A) and Recover(A).
        let newer = if let Some(next) = next {
            Some(
                control
                    .bind(SavedSessionId::parse(next).unwrap())
                    .await
                    .unwrap(),
            )
        } else {
            control
                .disconnect(Some(first.binding_id.clone()))
                .await
                .unwrap();
            None
        };
        assert_eq!(
            control
                .recover(first.session_id, first.binding_id)
                .await
                .unwrap(),
            BindingRecovery::Stale
        );
        if let Some(newer) = newer {
            assert_eq!(
                control
                    .check(newer.session_id, newer.binding_id)
                    .await
                    .unwrap(),
                BindingStatus::Active
            );
        }
        running.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn fenced_recovery_caches_success_before_a_lost_response() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let first = ready(&control, "saved-one").await;
    request(&control.sender, |reply| {
        Command::FaultedRenew(PersistFailure::AfterRename, reply)
    })
    .await
    .unwrap_err();
    let (reply, received) = oneshot::channel();
    drop(received);
    control
        .sender
        .send(Command::Recover(
            first.session_id.clone(),
            first.binding_id.clone(),
            reply,
        ))
        .await
        .unwrap();
    let result = control
        .recover(first.session_id.clone(), first.binding_id.clone())
        .await
        .unwrap();
    let BindingRecovery::Active(binding) = result.clone() else {
        panic!("lost-response retry must confirm its own generation");
    };
    assert_ne!(binding.binding_id, first.binding_id);
    assert_eq!(
        control
            .recover(first.session_id, first.binding_id)
            .await
            .unwrap(),
        result
    );
    control
        .disconnect(Some(binding.binding_id.clone()))
        .await
        .unwrap();
    assert_eq!(
        control
            .recover(binding.session_id, binding.binding_id)
            .await
            .unwrap(),
        BindingRecovery::Stale
    );
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn fenced_recovery_never_revives_an_uncertain_explicit_lifecycle_bind() {
    let directory = TempDir::new().unwrap();
    let session = OmpSession::open(&path(&directory)).await.unwrap();
    let control = session.control();
    let running = session.start(
        ProtocolOutput::new(tokio::io::sink()),
        CancellationToken::new(),
    );
    let first = ready(&control, "saved-one").await;
    request(&control.sender, |reply| {
        Command::FaultedBind(
            SavedSessionId::parse("saved-two").unwrap(),
            PersistFailure::AfterRename,
            reply,
        )
    })
    .await
    .unwrap_err();
    assert_eq!(
        control
            .check(first.session_id.clone(), first.binding_id.clone())
            .await
            .unwrap(),
        BindingStatus::Stale
    );
    assert_eq!(
        control
            .recover(first.session_id, first.binding_id)
            .await
            .unwrap(),
        BindingRecovery::Stale
    );
    running.shutdown().await.unwrap();
}

#[test]
fn fenced_recovery_wire_has_exact_private_discriminants_and_no_bound_field() {
    let binding = OmpBinding {
        session_id: SavedSessionId::parse("saved-one").unwrap(),
        binding_id: BindingId::generate(),
    };
    for (value, state) in [
        (BindingRecovery::Active(binding.clone()), "active"),
        (BindingRecovery::Inactive(binding.clone()), "inactive"),
    ] {
        assert_eq!(
            serde_json::to_value(value).unwrap(),
            json!({
                "state": state, "session_id": binding.session_id, "binding_id": binding.binding_id,
            })
        );
    }
    for (status, state) in [
        (BindingStatus::Active, "active"),
        (BindingStatus::Inactive, "inactive"),
        (BindingStatus::Stale, "stale"),
        (BindingStatus::Unavailable, "unavailable"),
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"state": state})
        );
    }
    assert_eq!(
        serde_json::to_value(BindingRecovery::Stale).unwrap(),
        json!({"state":"stale"})
    );
    assert_eq!(
        serde_json::to_value(BindingRecovery::Unavailable).unwrap(),
        json!({"state":"unavailable"})
    );
}
