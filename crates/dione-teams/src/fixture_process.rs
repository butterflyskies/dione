use super::{
    edge::{ClientSecretTokenProvider, tests::ConnectorFixture},
    runtime::{TeamsResidentBridge, TeamsRuntimeError, TeamsRuntimeErrorKind},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
use dione_messages::timestamp::Timestamp;
use dione_provider_delivery::{
    ProviderEventKey, ReplyHandle, TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError,
    TeamsDeliveryErrorKind, TeamsEventSink, TeamsResidentEvent,
};
use dione_teams_identities::{ActivityId, ActorId, BotId, ConversationId, TenantId};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct WireEvent {
    pub(crate) provider_key: ProviderEventKey,
    pub(crate) content: String,
    pub(crate) message_id: ActivityId,
    pub(crate) conversation_id: ConversationId,
    pub(crate) channel_id: String,
    pub(crate) user_id: ActorId,
    pub(crate) bot_id: BotId,
    pub(crate) tenant_id: TenantId,
    pub(crate) reply_handle: ReplyHandle,
    pub(crate) timestamp: Timestamp,
}

impl From<TeamsResidentEvent> for WireEvent {
    fn from(event: TeamsResidentEvent) -> Self {
        Self {
            provider_key: event.provider_key,
            content: event.content,
            message_id: event.message_id,
            conversation_id: event.conversation_id,
            channel_id: event.channel_id,
            user_id: event.user_id,
            bot_id: event.bot_id,
            tenant_id: event.tenant_id,
            reply_handle: event.reply_handle,
            timestamp: event.timestamp,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum WireAdmission {
    Committed,
    Duplicate,
    RouteUnavailable,
    CompletionUnknown { cause_class: RemoteCause },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Ready {
    pub(crate) control_url: String,
    pub(crate) ingress_url: String,
    pub(crate) authorization: String,
    pub(crate) activity: Vec<u8>,
    pub(crate) now: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReplyRequest {
    pub(crate) reply_handle: ReplyHandle,
    pub(crate) text: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum ReplyResult {
    Sent {
        incoming_activity_id: ActivityId,
        outgoing_activity_id: ActivityId,
        conversation_id: ConversationId,
    },
    UnknownReplyHandle,
    HttpUncertain,
    DecodeUncertain,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) token_requests: usize,
    pub(crate) replies: Vec<(String, serde_json::Value)>,
    pub(crate) pending_count: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum RemoteCause {
    JoinPanic,
    ResponseLost,
}

#[derive(Debug, thiserror::Error)]
#[error(
    "remote admission completion unknown ({cause_class:?}); original cause remains in application process"
)]
struct RemoteCompletionUnknown {
    cause_class: RemoteCause,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct BeginAdmission {
    pub(crate) operation_id: u64,
    pub(crate) authorization: String,
    pub(crate) activity: Vec<u8>,
    pub(crate) now: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum AdmissionResult {
    Admitted,
    RouteUnavailable,
    CompletionUnknown,
    TenantDenied,
    ConversationDenied,
}

struct CallbackSink {
    client: reqwest::Client,
    callback: String,
}

impl TeamsEventSink for CallbackSink {
    fn deliver<'a>(
        &'a self,
        event: TeamsResidentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>>
    {
        Box::pin(async move {
            let outcome: WireAdmission = self
                .client
                .post(format!("{}/deliver", self.callback))
                .json(&WireEvent::from(event))
                .send()
                .await
                .expect("application admission callback responds")
                .error_for_status()
                .expect("application callback classifies admission")
                .json()
                .await
                .expect("application returns supported admission classification");
            match outcome {
                WireAdmission::Committed => Ok(TeamsAdmissionOutcome::Committed),
                WireAdmission::Duplicate => Ok(TeamsAdmissionOutcome::Duplicate),
                WireAdmission::RouteUnavailable => Err(TeamsDeliveryError {
                    kind: TeamsDeliveryErrorKind::RouteUnavailable(Box::new(
                        std::io::Error::other(
                            "actual application route unavailable across test IPC",
                        ),
                    )),
                }),
                WireAdmission::CompletionUnknown { cause_class } => Ok(
                    TeamsAdmissionOutcome::CompletionUnknown(TeamsDeliveryError {
                        kind: TeamsDeliveryErrorKind::Storage(Box::new(RemoteCompletionUnknown {
                            cause_class,
                        })),
                    }),
                ),
            }
        })
    }
}

type NativeBridge = TeamsResidentBridge<ClientSecretTokenProvider>;
type AdmissionTask = tokio::task::JoinHandle<Result<(), TeamsRuntimeError>>;

struct ServiceState {
    fixture: ConnectorFixture,
    bridge: tokio::sync::RwLock<Option<Arc<NativeBridge>>>,
    operations: tokio::sync::Mutex<HashMap<u64, AdmissionTask>>,
    path: camino::Utf8PathBuf,
    client: reqwest::Client,
    callback: String,
    cancel: CancellationToken,
}

impl ServiceState {
    async fn current_bridge(&self) -> Arc<NativeBridge> {
        self.bridge
            .read()
            .await
            .as_ref()
            .expect("native bridge is open")
            .clone()
    }

    async fn open_bridge(&self) -> Arc<NativeBridge> {
        Arc::new(
            TeamsResidentBridge::new_durable(
                self.fixture.edge.clone(),
                TeamsDelivery::new(CallbackSink {
                    client: self.client.clone(),
                    callback: self.callback.clone(),
                }),
                &self.path,
            )
            .await
            .expect("same private native authority directory reopens"),
        )
    }
}

async fn ingress(
    State(state): State<Arc<ServiceState>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> impl axum::response::IntoResponse {
    let bridge = state.current_bridge().await;
    super::runtime::messages(State(bridge), headers, body).await
}

async fn reply(
    State(state): State<Arc<ServiceState>>,
    Json(request): Json<ReplyRequest>,
) -> Json<ReplyResult> {
    let bridge = state.current_bridge().await;
    let result = match bridge.reply(&request.reply_handle, &request.text).await {
        Ok(receipt) => ReplyResult::Sent {
            incoming_activity_id: receipt.incoming_activity_id,
            outgoing_activity_id: receipt.outgoing_activity_id,
            conversation_id: receipt.conversation_id,
        },
        Err(error) => match error.kind {
            TeamsRuntimeErrorKind::UnknownReplyHandle => ReplyResult::UnknownReplyHandle,
            TeamsRuntimeErrorKind::ReplyOutcomeUncertain(super::edge::TeamsEdgeError {
                kind: super::edge::TeamsEdgeErrorKind::Http(source),
            }) => {
                if source.is_decode() {
                    ReplyResult::DecodeUncertain
                } else {
                    ReplyResult::HttpUncertain
                }
            }
            other => panic!("unexpected native reply outcome in process test: {other:?}"),
        },
    };
    Json(result)
}

async fn snapshot(State(state): State<Arc<ServiceState>>) -> Json<Snapshot> {
    let persisted: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(state.path.join("teams-reply-authorities.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    Json(Snapshot {
        token_requests: state.fixture.token_requests(),
        replies: state.fixture.replies(),
        pending_count: persisted["pending"].as_object().unwrap().len(),
    })
}

async fn begin(
    State(state): State<Arc<ServiceState>>,
    Json(request): Json<BeginAdmission>,
) -> Json<bool> {
    let bridge = state.current_bridge().await;
    let mut operations = state.operations.lock().await;
    assert!(
        !operations.contains_key(&request.operation_id),
        "operation IDs are unique"
    );
    let task = tokio::spawn(async move {
        bridge
            .admit(&request.authorization, &request.activity, request.now)
            .await
    });
    assert!(operations.insert(request.operation_id, task).is_none());
    Json(true)
}

fn classify_admission(result: Result<(), TeamsRuntimeError>) -> AdmissionResult {
    match result {
        Ok(()) => AdmissionResult::Admitted,
        Err(error) => match error.kind {
            TeamsRuntimeErrorKind::Resident(TeamsDeliveryError {
                kind: TeamsDeliveryErrorKind::RouteUnavailable(_),
            }) => AdmissionResult::RouteUnavailable,
            TeamsRuntimeErrorKind::ResidentCompletionUnknown(TeamsDeliveryError {
                kind: TeamsDeliveryErrorKind::Storage(source),
            }) => {
                assert!(
                    source.downcast_ref::<RemoteCompletionUnknown>().is_some(),
                    "only explicit remote provenance travels through the IPC adapter"
                );
                AdmissionResult::CompletionUnknown
            }
            TeamsRuntimeErrorKind::Edge(super::edge::TeamsEdgeError {
                kind: super::edge::TeamsEdgeErrorKind::Probe(error),
            }) => match error.kind {
                super::ProbeErrorKind::TenantDenied => AdmissionResult::TenantDenied,
                super::ProbeErrorKind::ConversationDenied => AdmissionResult::ConversationDenied,
                other => panic!("unexpected native admission rejection: {other:?}"),
            },
            other => panic!("unexpected native admission outcome: {other:?}"),
        },
    }
}

async fn wait(
    State(state): State<Arc<ServiceState>>,
    Json(id): Json<u64>,
) -> Json<AdmissionResult> {
    let task = state
        .operations
        .lock()
        .await
        .remove(&id)
        .expect("admission operation exists");
    Json(classify_admission(
        task.await
            .expect("native admission operation does not panic"),
    ))
}

async fn cancel_admission(
    State(state): State<Arc<ServiceState>>,
    Json(id): Json<u64>,
) -> Json<bool> {
    let task = state
        .operations
        .lock()
        .await
        .remove(&id)
        .expect("admission operation exists");
    task.abort();
    assert!(
        task.await.unwrap_err().is_cancelled(),
        "native admission caller is actually cancelled"
    );
    Json(true)
}

async fn close(State(state): State<Arc<ServiceState>>) -> Json<bool> {
    assert!(
        state.operations.lock().await.is_empty(),
        "all admission operations must be settled before close"
    );
    let bridge = state
        .bridge
        .write()
        .await
        .take()
        .expect("native bridge was open");
    bridge.drain_replies().await;
    assert_eq!(
        Arc::strong_count(&bridge),
        1,
        "native HTTP handlers and operation tasks are quiescent"
    );
    drop(bridge);
    Json(true)
}

async fn reopen(State(state): State<Arc<ServiceState>>) -> Json<bool> {
    let mut current = state.bridge.write().await;
    assert!(current.is_none(), "close native bridge before reopening");
    *current = Some(state.open_bridge().await);
    Json(true)
}

async fn shutdown(State(state): State<Arc<ServiceState>>) {
    assert!(
        state.operations.lock().await.is_empty(),
        "no pending admissions at shutdown"
    );
    state.cancel.cancel();
}

#[tokio::test]
#[ignore = "private native fixture service explicitly launched by application boundary tests"]
async fn native_fixture_service() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let callback = std::env::var("DIONE_TEAMS_CALLBACK")
            .expect("application supplies its bounded admission callback");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let fixture = ConnectorFixture::start().await;
        let path = camino::Utf8PathBuf::from_path_buf(
            std::env::var_os("DIONE_TEAMS_STATE")
                .expect("parent owns private temporary native state")
                .into(),
        )
        .unwrap();
        let native_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let activity_id = std::env::var("DIONE_TEAMS_ACTIVITY")
            .unwrap_or_else(|_| "process-boundary-activity".into());
        let ready = Ready {
            control_url: format!("http://{}", control.local_addr().unwrap()),
            ingress_url: format!(
                "http://{}/api/messages",
                native_listener.local_addr().unwrap()
            ),
            authorization: fixture.authorization(),
            activity: fixture.activity(&activity_id),
            now: fixture.now(),
        };
        let cancel = CancellationToken::new();
        let state = Arc::new(ServiceState {
            fixture,
            bridge: tokio::sync::RwLock::new(None),
            operations: tokio::sync::Mutex::new(HashMap::new()),
            path,
            client: client.clone(),
            callback: callback.clone(),
            cancel: cancel.clone(),
        });
        *state.bridge.write().await = Some(state.open_bridge().await);
        let app = Router::new()
            .route("/api/messages", post(ingress))
            .layer(DefaultBodyLimit::max(256 * 1024))
            .with_state(state.clone());
        let ingress_cancel = cancel.clone();
        let ingress_task = tokio::spawn(async move {
            axum::serve(native_listener, app)
                .with_graceful_shutdown(ingress_cancel.cancelled_owned())
                .await
                .unwrap();
        });
        let app = Router::new()
            .route("/reply", post(reply))
            .route("/snapshot", post(snapshot))
            .route("/begin", post(begin))
            .route("/wait", post(wait))
            .route("/cancel", post(cancel_admission))
            .route("/close", post(close))
            .route("/reopen", post(reopen))
            .route("/shutdown", post(shutdown))
            .with_state(state.clone());
        let control_cancel = cancel.clone();
        let control_task = tokio::spawn(async move {
            axum::serve(control, app)
                .with_graceful_shutdown(control_cancel.cancelled_owned())
                .await
                .unwrap();
        });
        client
            .post(format!("{callback}/ready"))
            .json(&ready)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        cancel.cancelled().await;
        if let Some(bridge) = state.bridge.write().await.take() {
            bridge.drain_replies().await;
        }
        ingress_task.await.unwrap();
        control_task.await.unwrap();
    })
    .await
    .expect("private native fixture service exceeds its bounded lifetime");
}
