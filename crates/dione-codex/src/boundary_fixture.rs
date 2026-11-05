use super::{
    CodexEventQueue, CodexThreadId, LegacyDiscordScope, queue::CodexResidentHandle,
    queue_codec::LegacyQueueCodec,
};
use axum::{Json, Router, extract::State, routing::post};
use camino::Utf8PathBuf;
use dione_durable::{EndpointId, PersistFailure, QueueCodec};
use dione_provider_delivery::{TeamsAdmissionOutcome, TeamsDeliveryErrorKind};
use serde_json::{Value, json};
use std::{future::IntoFuture, sync::Arc, time::Duration};
use tokio::sync::{Mutex, watch};

struct Owner {
    path: Utf8PathBuf,
    queue: Option<CodexEventQueue>,
    connection: Option<dione_harness::HarnessConnection>,
    binding: watch::Receiver<Option<EndpointId>>,
    resident: Option<CodexResidentHandle>,
    reached: Option<tokio::sync::oneshot::Receiver<()>>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
    binding_reached: Option<tokio::sync::oneshot::Receiver<()>>,
    binding_release: Option<tokio::sync::oneshot::Sender<()>>,
    completion_reached: Option<tokio::sync::oneshot::Receiver<()>>,
    completion_release: Option<tokio::sync::oneshot::Sender<()>>,
    admission: Option<(u64, tokio::task::JoinHandle<Value>)>,
    genuine_panics: usize,
    cancelled: bool,
    shutdown: tokio_util::sync::CancellationToken,
}

async fn command(
    State(state): State<Arc<Mutex<Owner>>>,
    Json(request): Json<Value>,
) -> Json<Value> {
    if request["command"] == "teams_wait" {
        let mut owner = state.lock().await;
        if owner.cancelled {
            return Json(json!({"kind":"cancelled","operation_id":request["operation_id"]}));
        }
        let (id, task) = owner.admission.as_ref().unwrap();
        assert_eq!(request["operation_id"].as_u64(), Some(*id));
        if !task.is_finished() {
            return Json(json!({"kind":"pending"}));
        }
        let (_, task) = owner.admission.take().unwrap();
        let result = task.await.unwrap();
        if result["kind"] == "owner_join_panic" {
            owner.genuine_panics += 1;
        }
        return Json(result);
    }
    if request["command"] == "teams_cancel" {
        let mut owner = state.lock().await;
        let (id, task) = owner.admission.take().unwrap();
        assert_eq!(request["operation_id"].as_u64(), Some(id));
        task.abort();
        assert!(
            task.await.unwrap_err().is_cancelled(),
            "actual owner admission caller cancelled before queue release"
        );
        owner.cancelled = true;
        return Json(json!(true));
    }
    if matches!(
        request["command"].as_str(),
        Some("teams_reached" | "binding_reached" | "completion_reached")
    ) {
        let reached = {
            let mut owner = state.lock().await;
            match request["command"].as_str().unwrap() {
                "teams_reached" => owner.reached.take().unwrap(),
                "binding_reached" => owner.binding_reached.take().unwrap(),
                "completion_reached" => owner.completion_reached.take().unwrap(),
                _ => unreachable!(),
            }
        };
        reached.await.unwrap();
        return Json(json!(true));
    }
    if request["command"] == "decode_attention" {
        let scope = LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
            dione_contracts::ProviderId::new(request["provider"].as_str().unwrap()).unwrap(),
            dione_contracts::ModuleSlotId::new(request["slot"].as_str().unwrap()).unwrap(),
        ));
        let decoded = crate::legacy_delivery::decode_attention(&request["payload"], &scope)
            .unwrap()
            .expect("actual managed payload retains an attention descriptor");
        return Json(crate::boundary_delivery::encode_descriptor(
            decoded.descriptor(),
        ));
    }
    let operation = request["command"].as_str().unwrap();
    if matches!(
        operation,
        "call"
            | "resident"
            | "fault"
            | "snapshot"
            | "retained"
            | "status"
            | "queued_payloads"
            | "enqueue_public"
            | "enqueue_live"
            | "resident_next"
            | "resident_ack"
            | "disk_bytes"
            | "watch_disk"
            | "snapshot_probe"
            | "pause_binding"
            | "pause_completion"
            | "deliver"
    ) {
        let (queue, connection, binding, resident, path) = {
            let owner = state.lock().await;
            (
                owner.queue.as_ref().unwrap().clone(),
                owner.connection.clone(),
                owner.binding.clone(),
                owner.resident.clone(),
                owner.path.clone(),
            )
        };
        let result = match operation {
            "call" => {
                let result = connection
                    .unwrap()
                    .protocol()
                    .call(request["name"].as_str().unwrap(), &request["args"])
                    .await
                    .expect("actual owner adapter handles advertised tools");
                match result {
                    Ok(value) => json!({"ok":value}),
                    Err(error) => json!({"error":error.to_string()}),
                }
            }
            "resident" => {
                let resident = queue.register_live_consumer().await.unwrap();
                let id = resident.consumer_id().as_str().to_owned();
                state.lock().await.resident = Some(resident);
                json!({"consumer_id":id})
            }
            "fault" => {
                let fault = match request["stage"].as_str() {
                    Some("BeforeWrite") => Some(PersistFailure::BeforeWrite),
                    Some("BeforeRename") => Some(PersistFailure::BeforeRename),
                    Some("AfterRename") => Some(PersistFailure::AfterRename),
                    None => None,
                    other => panic!("unexpected fault {other:?}"),
                };
                queue.test_set_persist_failure(fault).await;
                json!(true)
            }
            "snapshot" => {
                let memory = queue.test_snapshot().await;
                let disk = LegacyQueueCodec::new(queue.legacy_scope().clone())
                    .decode(&std::fs::read(path.join("codex-inbox.json")).unwrap())
                    .unwrap();
                json!({"watch":binding.borrow().as_ref().map(EndpointId::as_str),"memory":memory.endpoint.as_ref().map(EndpointId::as_str),"disk":disk.endpoint.as_ref().map(EndpointId::as_str)})
            }
            "retained" => {
                queue.test_set_persist_failure(None).await;
                queue.enqueue(json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"retained binding","meta":{"message_id":"1","chat_id":"123"}}})).await.unwrap();
                let thread =
                    CodexThreadId::parse(binding.borrow().as_ref().unwrap().as_str()).unwrap();
                let event = resident
                    .unwrap()
                    .next_event(&thread, Duration::ZERO, Duration::from_secs(120))
                    .await
                    .unwrap()
                    .expect("delivery without another bind or enqueue");
                assert_eq!(event.event["params"]["content"], "retained binding");
                event.event
            }
            "status" => json!(queue.status().await),
            "queued_payloads" => json!(
                queue
                    .test_snapshot()
                    .await
                    .entries
                    .into_iter()
                    .map(|entry| entry.payload.into_value())
                    .collect::<Vec<_>>()
            ),
            "enqueue_public" => match queue.enqueue(request["event"].clone()).await {
                Ok(value) => json!({"ok":value}),
                Err(error) => json!({"error":error.to_string()}),
            },
            "enqueue_live" => {
                let key = dione_provider_delivery::ProviderEventKey::teams(
                    "tenant-a".parse().unwrap(),
                    "bot-a".parse().unwrap(),
                    "conversation-a".parse().unwrap(),
                    request["activity_id"].as_str().unwrap().parse().unwrap(),
                );
                assert!(matches!(
                    queue
                        .enqueue_live(request["event"].clone(), key)
                        .await
                        .unwrap(),
                    crate::LiveAdmissionOutcome::Committed
                ));
                json!({"kind":"committed"})
            }
            "resident_next" => {
                let thread = request["thread"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| binding.borrow().as_ref().unwrap().as_str().to_owned());
                let thread = CodexThreadId::parse(&thread).unwrap();
                json!(
                    resident
                        .unwrap()
                        .next_event(
                            &thread,
                            Duration::from_secs(request["wait_seconds"].as_u64().unwrap()),
                            Duration::from_secs(request["lease_seconds"].as_u64().unwrap())
                        )
                        .await
                        .unwrap()
                )
            }
            "resident_ack" => {
                resident
                    .unwrap()
                    .acknowledge(
                        &crate::DeliveryToken::parse(request["token"].as_str().unwrap()).unwrap(),
                    )
                    .await
                    .unwrap();
                json!(true)
            }
            "disk_bytes" => json!(std::fs::read(path.join("codex-inbox.json")).unwrap()),
            "watch_disk" => {
                let disk = LegacyQueueCodec::new(queue.legacy_scope().clone())
                    .decode(&std::fs::read(path.join("codex-inbox.json")).unwrap())
                    .unwrap();
                json!({"watch":binding.borrow().as_ref().map(EndpointId::as_str),"disk":disk.endpoint.as_ref().map(EndpointId::as_str)})
            }
            "snapshot_probe" => {
                json!({"blocked":tokio::time::timeout(Duration::from_millis(request["timeout_ms"].as_u64().unwrap()),queue.test_snapshot()).await.is_err()})
            }
            "deliver" => {
                let delivery = crate::boundary_delivery::decode(&request["delivery"]);
                assert_eq!(
                    crate::boundary_delivery::encode(&delivery),
                    request["delivery"],
                    "descriptive delivery survives the private process transport"
                );
                let observed_source =
                    crate::boundary_delivery::encode(&delivery)["messages"][0]["source"].clone();
                match connection.unwrap().deliver(&delivery).await {
                    Ok(()) => json!({"kind":"ok"}),
                    Err(error) => {
                        assert!(
                            error.completion_is_unknown(),
                            "actual owner delivery must classify lost completion"
                        );
                        json!({"kind":"completion_unknown","source":observed_source})
                    }
                }
            }
            "pause_completion" => {
                let (reached, release) = queue
                    .pause_owned_completion_for_test(request["lose_response"].as_bool().unwrap());
                let mut owner = state.lock().await;
                owner.completion_reached = Some(reached);
                owner.completion_release = Some(release);
                json!(true)
            }
            "pause_binding" => {
                let (reached, release) = queue.pause_binding_publish_for_test();
                let mut owner = state.lock().await;
                owner.binding_reached = Some(reached);
                owner.binding_release = Some(release);
                json!(true)
            }
            _ => unreachable!("listed owner operation"),
        };
        return Json(result);
    }
    let mut owner = state.lock().await;
    let result = match request["command"].as_str().unwrap() {
        "teams_setup" => {
            owner.resident = Some(
                owner
                    .queue
                    .as_ref()
                    .unwrap()
                    .register_live_consumer()
                    .await
                    .unwrap(),
            );
            owner
                .resident
                .as_ref()
                .unwrap()
                .bind_live_thread(Some(CodexThreadId::parse("owned-admission").unwrap()))
                .await
                .unwrap();
            let (reached, release) = owner
                .queue
                .as_ref()
                .unwrap()
                .pause_owned_completion_for_test(request["lose_response"].as_bool().unwrap());
            owner.reached = Some(reached);
            owner.release = Some(release);
            json!(true)
        }
        "teams_begin" => {
            let id = request["operation_id"].as_u64().unwrap();
            let event = &request["event"];
            let event = dione_provider_delivery::TeamsResidentEvent {
                provider_key: serde_json::from_value(event["provider_key"].clone()).unwrap(),
                content: serde_json::from_value(event["content"].clone()).unwrap(),
                message_id: serde_json::from_value(event["message_id"].clone()).unwrap(),
                conversation_id: serde_json::from_value(event["conversation_id"].clone()).unwrap(),
                channel_id: serde_json::from_value(event["channel_id"].clone()).unwrap(),
                user_id: serde_json::from_value(event["user_id"].clone()).unwrap(),
                bot_id: serde_json::from_value(event["bot_id"].clone()).unwrap(),
                tenant_id: serde_json::from_value(event["tenant_id"].clone()).unwrap(),
                reply_handle: serde_json::from_value(event["reply_handle"].clone()).unwrap(),
                timestamp: serde_json::from_value(event["timestamp"].clone()).unwrap(),
            };
            assert!(owner.admission.is_none());
            owner.cancelled = false;
            let delivery = crate::teams_delivery(owner.queue.as_ref().unwrap().clone());
            owner.admission = Some((
                id,
                tokio::spawn(async move {
                    match delivery.deliver(event).await {
                        Ok(TeamsAdmissionOutcome::Committed) => {
                            json!({"kind":"committed","operation_id":id})
                        }
                        Ok(TeamsAdmissionOutcome::Duplicate) => {
                            json!({"kind":"duplicate","operation_id":id})
                        }
                        Ok(TeamsAdmissionOutcome::CompletionUnknown(error)) => {
                            let TeamsDeliveryErrorKind::CompletionUnknown(source) = error.kind
                            else {
                                panic!("actual owner JoinError required");
                            };
                            assert!(
                                source.is_panic(),
                                "genuine owner completion source must be a panic before serialization"
                            );
                            json!({"kind":"owner_join_panic","operation_id":id})
                        }
                        Ok(TeamsAdmissionOutcome::CommittedDurabilityUncertain(error)) => {
                            panic!("unexpected durable uncertainty: {error:?}")
                        }
                        Err(error) => {
                            let TeamsDeliveryErrorKind::RouteUnavailable(source) = error.kind
                            else {
                                panic!("unexpected actual owner rejection category");
                            };
                            assert!(
                                source.downcast_ref::<crate::LiveQueueError>().is_some_and(
                                    |error| matches!(
                                        error.kind,
                                        crate::LiveQueueErrorKind::ResidentUnavailable(_)
                                    )
                                ),
                                "actual typed resident-unavailable source required"
                            );
                            json!({"kind":"route_unavailable","operation_id":id})
                        }
                    }
                }),
            ));
            json!(true)
        }
        "teams_release" | "binding_release" | "completion_release" => {
            let release = match operation {
                "teams_release" => owner.release.take().unwrap(),
                "binding_release" => owner.binding_release.take().unwrap(),
                "completion_release" => owner.completion_release.take().unwrap(),
                _ => unreachable!(),
            };
            release.send(()).unwrap();
            json!(true)
        }
        "teams_drain" => {
            owner.queue.as_ref().unwrap().drain_owned().await;
            json!(true)
        }
        "teams_restart" => {
            assert!(owner.admission.is_none());
            owner.queue.as_ref().unwrap().drain_owned().await;
            let consumer_id = owner.resident.as_ref().unwrap().consumer_id().clone();
            let scope = owner.queue.as_ref().unwrap().legacy_scope().clone();
            drop(owner.resident.take());
            drop(owner.connection.take());
            drop(owner.queue.take());
            let queue = CodexEventQueue::open(&owner.path, scope).await.unwrap();
            let resident = queue.register_live_consumer().await.unwrap();
            assert_eq!(
                resident.consumer_id(),
                &consumer_id,
                "actual durable resident identity survives reopen"
            );
            let event = resident
                .next_event(
                    &CodexThreadId::parse("owned-admission").unwrap(),
                    Duration::ZERO,
                    Duration::from_secs(30),
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                event.event["params"]["meta"]["message_id"],
                "owned/admission"
            );
            let handle = event.event["params"]["meta"]["reply_handle"].clone();
            owner.queue = Some(queue);
            owner.resident = Some(resident);
            json!({"handle":handle,"message_id":"owned/admission","consumer_id":consumer_id.as_str(),"genuine_panics":owner.genuine_panics})
        }
        "tools" => json!(owner.connection.as_ref().unwrap().protocol().tools()),
        "legacy_native_fixture" => {
            let queue = owner.queue.as_ref().unwrap();
            let scope = queue.legacy_scope().clone();
            drop(owner.resident.take());
            drop(owner.connection.take());
            drop(owner.queue.take());
            let path = owner.path.join("codex-inbox.json");
            let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            disk["next_consumer_generation"] = json!(1);
            disk["primary_consumer"] = json!("codex-consumer-0");
            disk["consumers"] = json!([{"id":"codex-consumer-0","label":"dione-live-app-server","expires_at":chrono::Utc::now()+chrono::TimeDelta::hours(1),"ttl_seconds":3600}]);
            std::fs::write(&path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();
            let queue = CodexEventQueue::load(&owner.path, scope).unwrap();
            let (binding_tx, binding) = watch::channel(None);
            owner.connection = Some(crate::connect(queue.clone(), binding_tx));
            owner.binding = binding;
            owner.queue = Some(queue);
            json!(true)
        }
        "legacy_native_primary_conflict" => {
            assert!(
                matches!(
                    owner.queue.as_ref().unwrap().register_live_consumer().await,
                    Err(crate::CodexQueueError {
                        kind: crate::CodexQueueErrorKind::Durable(dione_durable::QueueError {
                            kind: dione_durable::QueueErrorKind::PrimaryConsumerExists,
                            ..
                        })
                    })
                ),
                "genuine owner-local primary-conflict cause required"
            );
            json!({"kind":"primary_consumer_exists"})
        }
        "legacy_native_recover" => {
            let scope = owner.queue.as_ref().unwrap().legacy_scope().clone();
            drop(owner.resident.take());
            drop(owner.connection.take());
            drop(owner.queue.take());
            let path = owner.path.join("codex-inbox.json");
            let mut disk: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            disk["consumers"][0]["expires_at"] =
                json!(chrono::Utc::now() - chrono::TimeDelta::seconds(1));
            std::fs::write(&path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();
            let queue = CodexEventQueue::load(&owner.path, scope).unwrap();
            let resident = queue.register_live_consumer().await.unwrap();
            let (binding_tx, binding) = watch::channel(None);
            owner.connection = Some(crate::connect(queue.clone(), binding_tx));
            owner.binding = binding;
            owner.queue = Some(queue);
            owner.resident = Some(resident);
            json!(true)
        }
        "expire_resident_persist_reload" => {
            let queue = owner.queue.as_ref().unwrap();
            queue
                .test_mutate_snapshot(|snapshot| {
                    snapshot.consumers[0].expires_at =
                        chrono::Utc::now() - chrono::TimeDelta::seconds(1)
                })
                .await;
            let snapshot = queue.test_snapshot().await;
            let bytes = LegacyQueueCodec::new(queue.legacy_scope().clone())
                .encode(&snapshot)
                .unwrap();
            std::fs::write(owner.path.join("codex-inbox.json"), bytes).unwrap();
            let scope = queue.legacy_scope().clone();
            drop(owner.resident.take());
            drop(owner.connection.take());
            drop(owner.queue.take());
            let queue = CodexEventQueue::load(&owner.path, scope).unwrap();
            let (binding_tx, binding) = watch::channel(None);
            owner.connection = Some(crate::connect(queue.clone(), binding_tx));
            owner.binding = binding;
            owner.queue = Some(queue);
            json!(true)
        }
        "unassign_live_persist" => {
            let queue = owner.queue.as_ref().unwrap();
            queue
                .test_mutate_snapshot(|snapshot| {
                    snapshot.primary_consumer = None;
                    snapshot.consumers.clear();
                    snapshot.entries[0].consumer_id = None;
                })
                .await;
            let snapshot = queue.test_snapshot().await;
            let bytes = LegacyQueueCodec::new(queue.legacy_scope().clone())
                .encode(&snapshot)
                .unwrap();
            std::fs::write(owner.path.join("codex-inbox.json"), bytes).unwrap();
            json!(true)
        }
        "shutdown" => {
            owner.shutdown.cancel();
            json!(true)
        }
        other => panic!("unexpected command {other}"),
    };
    Json(result)
}

#[tokio::test]
#[ignore]
async fn private_owner_fixture_service() {
    let path = Utf8PathBuf::from(std::env::var("DIONE_CODEX_BOUNDARY_STATE").unwrap());
    let scope = LegacyDiscordScope::new(dione_contracts::ModuleInstanceId::new(
        dione_contracts::ProviderId::new("discord").unwrap(),
        dione_contracts::ModuleSlotId::new("primary").unwrap(),
    ));
    let queue = CodexEventQueue::open(&path, scope).await.unwrap();
    let (binding_tx, binding) = watch::channel(None);
    let connection = crate::connect(queue.clone(), binding_tx);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let state = Arc::new(Mutex::new(Owner {
        path: path.clone(),
        queue: Some(queue),
        connection: Some(connection),
        binding,
        resident: None,
        reached: None,
        release: None,
        admission: None,
        binding_reached: None,
        binding_release: None,
        completion_reached: None,
        completion_release: None,
        genuine_panics: 0,
        cancelled: false,
        shutdown: shutdown.clone(),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::write(
        path.join("ready.tmp"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    std::fs::rename(path.join("ready.tmp"), path.join("ready")).unwrap();
    tokio::time::timeout(
        Duration::from_secs(90),
        axum::serve(
            listener,
            Router::new().route("/", post(command)).with_state(state),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .into_future(),
    )
    .await
    .unwrap()
    .unwrap();
}
