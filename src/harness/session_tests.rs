use super::*;
use crate::{
    attention::delivery::{AttentionDeliveryReceipt, AttentionGuardFailure},
    delivery::ProviderEventKey,
    teams::{
        ReplyHandle,
        delivery::{TeamsAdmissionOutcome, TeamsResidentEvent},
    },
    timestamp::Timestamp,
};
use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{error::Error, time::Duration};
use tempfile::TempDir;
use tokio::{
    net::UnixListener,
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Debug)]
pub(crate) struct Guard;
impl AttentionDeliveryGuard for Guard {
    fn check(
        &self,
        _: &crate::attention::delivery::AttentionDescriptor<'_>,
    ) -> Result<(), AttentionGuardFailure> {
        Ok(())
    }
    fn receipt<'a>(
        self: Arc<Self>,
        _: &'a crate::attention::types::RecordId,
        _: AttentionDeliveryReceipt,
    ) -> BoxFuture<'a, Result<(), crate::attention::delivery::AttentionReceiptError>> {
        Box::pin(async { Ok(()) })
    }
}

fn event(content: &str) -> crate::core::delivery::Delivery {
    crate::harness::test_support::text(content)
}

pub(crate) fn peer(
    listener: UnixListener,
    barrier: Arc<Notify>,
) -> (JoinHandle<()>, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            while let Some(Ok(message)) = socket.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id") else {
                    continue;
                };
                if request["method"] == "initialize" {
                    tx.send(request.clone()).unwrap();
                    barrier.notified().await;
                }
                let result = if request["method"] == "thread/read" {
                    json!({"thread":{"status":{"type":"idle"},"turns":[]}})
                } else {
                    json!({})
                };
                if socket
                    .send(Message::Text(json!({"id":id,"result":result}).to_string()))
                    .await
                    .is_err()
                {
                    break;
                }
                if request["method"] == "turn/start" {
                    tx.send(request).unwrap();
                }
            }
        }
    });
    (task, rx)
}

pub(crate) async fn receive(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("session must deliver through its worker")
        .expect("peer remains open")
}

#[tokio::test]
async fn harness_session_delivery_survives_process_cancellation_and_rebinds() {
    let dir = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let socket = root.join("app.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let session = HarnessSession::codex(
        &root,
        CodexSessionOptions::new(
            Some(socket),
            Some("first-thread".parse().unwrap()),
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    let connection = session.connection();
    let teams = crate::composition::teams_admission::from_codex(session.codex_admission().unwrap());
    connection.deliver(&event("before start")).await.unwrap();
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    let barrier = Arc::new(Notify::new());
    let (peer, mut received) = peer(listener, barrier.clone());
    let process = CancellationToken::new();
    let delivery = DeliveryConfig {
        preamble_mode: crate::config::PreambleMode::Always,
        preamble_template: crate::config::PreambleTemplate::new("session preamble"),
        ..DeliveryConfig::default()
    };
    let running = session
        .start(&delivery, Arc::new(Guard), process.clone())
        .await
        .unwrap();
    wait_for_worker(&connection).await;
    connection
        .deliver(&event("first live event"))
        .await
        .unwrap();
    assert_eq!(receive(&mut received).await["method"], "initialize");
    process.cancel();
    barrier.notify_one();
    let first = receive(&mut received).await;
    assert_eq!(first["params"]["threadId"], "first-thread");
    assert!(first.to_string().contains("first live event"));
    assert!(first.to_string().contains("session preamble"));
    let status = connection
        .protocol()
        .call("event_queue_status", &json!({}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status["unassigned"], 1,
        "startup must not replay historical backlog"
    );
    connection
        .deliver(&event("after process cancellation"))
        .await
        .unwrap();
    let second = receive(&mut received).await;
    assert!(second.to_string().contains("after process cancellation"));
    let bound = connection
        .protocol()
        .call("bind_codex_thread", &json!({"thread_id":"second-thread"}))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bound, json!({"bound":true,"thread_id":"second-thread"}));
    let receipt = teams
        .deliver(TeamsResidentEvent {
            provider_key: ProviderEventKey::teams(
                "tenant".parse().unwrap(),
                "bot".parse().unwrap(),
                "conversation".parse().unwrap(),
                "activity".parse().unwrap(),
            ),
            content: "Teams after rebind".into(),
            message_id: "activity".parse().unwrap(),
            conversation_id: "conversation".parse().unwrap(),
            channel_id: "channel".into(),
            user_id: "user".parse().unwrap(),
            bot_id: "bot".parse().unwrap(),
            tenant_id: "tenant".parse().unwrap(),
            reply_handle: ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
            timestamp: Timestamp::now(),
        })
        .await
        .unwrap();
    assert!(matches!(receipt, TeamsAdmissionOutcome::Committed));
    assert_eq!(receive(&mut received).await["method"], "initialize");
    barrier.notify_one();
    let third = receive(&mut received).await;
    assert_eq!(third["params"]["threadId"], "second-thread");
    assert!(third.to_string().contains("Teams after rebind"));
    assert!(
        third
            .to_string()
            .contains("teams-00000000-0000-4000-8000-000000000001")
    );
    running.shutdown().await.unwrap();
    assert!(
        crate::codex::CodexEventQueue::load(
            &root,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap()
            ))
        )
        .is_err()
    );
    drop(connection);
    drop(teams);
    assert!(
        crate::codex::CodexEventQueue::load(
            &root,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap()
            ))
        )
        .is_ok()
    );
    peer.abort();
}

#[tokio::test]
async fn harness_session_keeps_lock_after_worker_failure_until_shutdown() {
    let dir = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let state = root.join("state");
    let moved = root.join("moved");
    let session = HarnessSession::codex(
        &state,
        CodexSessionOptions::new(
            Some(root.join("unused.sock")),
            Some("thread".parse().unwrap()),
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    let process = CancellationToken::new();
    let running = session
        .start(&DeliveryConfig::default(), Arc::new(Guard), process.clone())
        .await
        .unwrap();
    std::fs::rename(&state, &moved).unwrap();
    std::fs::write(&state, b"prevent queue persistence").unwrap();
    timeout(Duration::from_secs(5), process.cancelled())
        .await
        .expect("worker failure requests process shutdown");
    assert!(
        crate::codex::CodexEventQueue::load(
            &moved,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap()
            ))
        )
        .is_err(),
        "running session retains moved inbox lock after worker exit"
    );
    running.shutdown().await.unwrap();
    assert!(
        crate::codex::CodexEventQueue::load(
            &moved,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap()
            ))
        )
        .is_ok()
    );
}

#[tokio::test]
async fn harness_session_errors_preserve_sources_and_teams_capability() {
    let session = HarnessSession::claude();
    let error = match session.codex_admission() {
        Ok(_) => panic!("Claude has no resident Teams delivery"),
        Err(error) => error,
    };
    assert_eq!(
        error.to_string(),
        "resident Teams ingress requires a prepared Codex or OMP session"
    );
    assert!(matches!(
        error.kind,
        HarnessSessionErrorKind::TeamsUnavailable
    ));
    assert!(
        error.source().is_none(),
        "capability error has no duplicate context"
    );
    let dir = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let owner = HarnessSession::codex(
        &root,
        CodexSessionOptions::new(
            None,
            None,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    let error = match HarnessSession::codex(
        &root,
        CodexSessionOptions::new(
            None,
            None,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    {
        Ok(_) => panic!("inbox lock must exclude another session"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "failed to open Codex event queue");
    assert_ne!(
        error.source().unwrap().to_string(),
        error.to_string(),
        "startup context must appear only once in the error chain"
    );
    let mut source = error.source();
    let mut lock_conflict = false;
    while let Some(error) = source {
        if matches!(
            error.downcast_ref::<CodexQueueError>(),
            Some(CodexQueueError {
                kind: crate::codex::CodexQueueErrorKind::Durable(crate::durable::QueueError {
                    kind: crate::durable::QueueErrorKind::Locked(_),
                    ..
                })
            })
        ) {
            lock_conflict = true;
            break;
        }
        source = error.source();
    }
    assert!(
        lock_conflict,
        "source chain retains the concrete inbox lock conflict"
    );
    drop(owner);
}

pub(crate) async fn wait_for_worker(connection: &HarnessConnection) {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = connection
                .protocol()
                .call("event_queue_status", &json!({}))
                .await
                .unwrap()
                .unwrap();
            if !status["primary_consumer"].is_null() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("worker registers its durable consumer");
}

#[tokio::test]
async fn oh_my_pi_session_is_direct_without_inbox_or_worker() {
    let dir = TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    let state = root.join("not-created");
    let session = crate::harness::HarnessSelection::OhMyPi
        .prepare(
            &state,
            CodexSessionOptions::new(
                Some(root.join("absent.sock")),
                None,
                crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                    crate::core::ProviderId::new("discord").unwrap(),
                    crate::core::ModuleSlotId::new("primary").unwrap(),
                )),
            ),
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        session.connection().attention_policy(),
        crate::harness::AttentionPolicy::Direct
    );
    let error = session.codex_admission().err().unwrap();
    assert!(matches!(
        error.kind,
        HarnessSessionErrorKind::TeamsUnavailable
    ));
    assert_eq!(
        error.to_string(),
        "resident Teams ingress requires a prepared Codex or OMP session"
    );
    let process = CancellationToken::new();
    let running = session
        .start(&DeliveryConfig::default(), Arc::new(Guard), process.clone())
        .await
        .unwrap();
    assert!(!process.is_cancelled());
    running.shutdown().await.unwrap();
    assert!(!state.exists());
    let codex = HarnessSession::codex(
        &state,
        CodexSessionOptions::new(
            None,
            None,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        ),
    )
    .await
    .unwrap();
    assert!(codex.codex_admission().is_ok());
    assert!(state.exists());
}
