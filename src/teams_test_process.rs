use axum::{Json, Router, extract::State, routing::post};
use dione_messages::timestamp::Timestamp;
use dione_provider_delivery::{
    ProviderEventKey, ReplyHandle, TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryErrorKind,
    TeamsResidentEvent,
};
use dione_teams::{
    edge::LiveReplyReceipt,
    runtime::{TeamsReplyAuthority, TeamsRuntimeError, TeamsRuntimeErrorKind},
};
use dione_teams_identities::{ActivityId, ActorId, BotId, ConversationId, TenantId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, RwLock, oneshot};

#[derive(Debug, Deserialize)]
struct WireEvent {
    provider_key: ProviderEventKey,
    content: String,
    message_id: ActivityId,
    conversation_id: ConversationId,
    channel_id: String,
    user_id: ActorId,
    bot_id: BotId,
    tenant_id: TenantId,
    reply_handle: ReplyHandle,
    timestamp: Timestamp,
}
impl WireEvent {
    fn into_event(self) -> TeamsResidentEvent {
        TeamsResidentEvent {
            provider_key: self.provider_key,
            content: self.content,
            message_id: self.message_id,
            conversation_id: self.conversation_id,
            channel_id: self.channel_id,
            user_id: self.user_id,
            bot_id: self.bot_id,
            tenant_id: self.tenant_id,
            reply_handle: self.reply_handle,
            timestamp: self.timestamp,
        }
    }
}

#[derive(Debug, Serialize)]
enum RemoteCause {
    JoinPanic,
    ResponseLost,
}
#[derive(Debug, Serialize)]
enum WireAdmission {
    Committed,
    Duplicate,
    RouteUnavailable,
    CompletionUnknown { cause_class: RemoteCause },
}
#[derive(Debug, Deserialize)]
struct Ready {
    control_url: String,
    ingress_url: String,
    authorization: String,
    activity: Vec<u8>,
    now: u64,
}
#[derive(Debug, Serialize)]
struct ReplyRequest<'a> {
    reply_handle: &'a ReplyHandle,
    text: &'a str,
}
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
pub(crate) struct Snapshot {
    pub(crate) token_requests: usize,
    pub(crate) replies: Vec<(String, Value)>,
    pub(crate) pending_count: usize,
}
#[derive(Debug, Serialize)]
struct BeginAdmission<'a> {
    operation_id: u64,
    authorization: &'a str,
    activity: &'a [u8],
    now: u64,
}
#[derive(Debug, Deserialize)]
pub(crate) enum AdmissionResult {
    Admitted,
    RouteUnavailable,
    CompletionUnknown,
    TenantDenied,
    ConversationDenied,
}

#[derive(Debug, thiserror::Error)]
#[error("remote Codex owner asserted a genuine mapped panic for operation {operation_id}")]
pub(crate) struct RemoteOwnerPanic {
    pub(crate) operation_id: u64,
}

struct Callback {
    delivery: RwLock<Option<TeamsDelivery>>,
    ready: Mutex<Option<oneshot::Sender<Ready>>>,
    join_panics: AtomicUsize,
    owner_join_panics: AtomicUsize,
    owner_operation: AtomicU64,
}
async fn ready(State(state): State<Arc<Callback>>, Json(value): Json<Ready>) {
    state
        .ready
        .lock()
        .await
        .take()
        .unwrap()
        .send(value)
        .unwrap();
}
async fn deliver(
    State(state): State<Arc<Callback>>,
    Json(event): Json<WireEvent>,
) -> Json<WireAdmission> {
    let current = state.delivery.read().await;
    let delivery = current.as_ref().expect("live application adapter");
    let outcome = match delivery.deliver(event.into_event()).await {
        Ok(TeamsAdmissionOutcome::Committed) => WireAdmission::Committed,
        Ok(TeamsAdmissionOutcome::Duplicate) => WireAdmission::Duplicate,
        Ok(TeamsAdmissionOutcome::CompletionUnknown(error)) => {
            let cause_class = match error.kind {
                TeamsDeliveryErrorKind::CompletionUnknown(source) => {
                    assert!(
                        source.is_panic(),
                        "actual queue completion must retain its panic classification before transport"
                    );
                    state.join_panics.fetch_add(1, Ordering::SeqCst);
                    RemoteCause::JoinPanic
                }
                TeamsDeliveryErrorKind::Storage(source) => {
                    let attestation = source
                        .downcast_ref::<RemoteOwnerPanic>()
                        .expect("explicit owner panic attestation, not a transport failure");
                    state
                        .owner_operation
                        .store(attestation.operation_id, Ordering::SeqCst);
                    state.owner_join_panics.fetch_add(1, Ordering::SeqCst);
                    RemoteCause::ResponseLost
                }
                TeamsDeliveryErrorKind::ResponseLost(_) => RemoteCause::ResponseLost,
                other => panic!("unexpected actual application completion source: {other:?}"),
            };
            WireAdmission::CompletionUnknown { cause_class }
        }
        Err(error) if matches!(error.kind, TeamsDeliveryErrorKind::RouteUnavailable(_)) => {
            WireAdmission::RouteUnavailable
        }
        _ => panic!("unsupported actual application admission outcome"),
    };
    Json(outcome)
}

struct ReplyProxy {
    client: reqwest::Client,
    control_url: String,
}
impl TeamsReplyAuthority for ReplyProxy {
    fn reply<'a>(
        &'a self,
        handle: &'a ReplyHandle,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReplyReceipt, TeamsRuntimeError>> + Send + 'a>>
    {
        Box::pin(async move {
            let result = self
                .client
                .post(format!("{}/reply", self.control_url))
                .json(&ReplyRequest {
                    reply_handle: handle,
                    text,
                })
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json::<ReplyResult>()
                .await
                .unwrap();
            match result {
                ReplyResult::Sent {
                    incoming_activity_id,
                    outgoing_activity_id,
                    conversation_id,
                } => Ok(LiveReplyReceipt {
                    incoming_activity_id,
                    outgoing_activity_id,
                    conversation_id,
                }),
                ReplyResult::UnknownReplyHandle => {
                    Err(TeamsRuntimeErrorKind::UnknownReplyHandle.into())
                }
                other => panic!("unexpected native tool outcome in this composed test: {other:?}"),
            }
        })
    }
}

struct FixtureChild(std::process::Child);
impl Drop for FixtureChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
struct FixtureCallbackTask(tokio::task::AbortHandle);
impl Drop for FixtureCallbackTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(crate) struct FixtureProcess {
    child: FixtureChild,
    callback_task: FixtureCallbackTask,
    _directory: tempfile::TempDir,
    callback: Arc<Callback>,
    client: reqwest::Client,
    ready: Ready,
}
impl FixtureProcess {
    pub(crate) async fn start(delivery: TeamsDelivery, activity_id: &str) -> Self {
        let executable = std::env::var_os("DIONE_TEAMS_FIXTURE_EXECUTABLE")
            .expect("verification setup supplies exact current native fixture compiler artifact");
        let (tx, rx) = oneshot::channel();
        let callback = Arc::new(Callback {
            delivery: RwLock::new(Some(delivery)),
            ready: Mutex::new(Some(tx)),
            join_panics: AtomicUsize::new(0),
            owner_join_panics: AtomicUsize::new(0),
            owner_operation: AtomicU64::new(0),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/ready", post(ready))
            .route("/deliver", post(deliver))
            .with_state(callback.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let callback_task = FixtureCallbackTask(task.abort_handle());
        let directory = tempfile::TempDir::new().unwrap();
        let child = FixtureChild(
            std::process::Command::new(executable)
                .args([
                    "--exact",
                    "fixture_process::native_fixture_service",
                    "--ignored",
                    "--nocapture",
                ])
                .env("DIONE_TEAMS_CALLBACK", url)
                .env("DIONE_TEAMS_ACTIVITY", activity_id)
                .env("DIONE_TEAMS_STATE", directory.path())
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let ready = tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .unwrap()
            .unwrap();
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            child,
            callback_task,
            _directory: directory,
            callback,
            client,
            ready,
        }
    }
    pub(crate) fn authority(&self) -> Arc<dyn TeamsReplyAuthority> {
        Arc::new(ReplyProxy {
            client: self.client.clone(),
            control_url: self.ready.control_url.clone(),
        })
    }
    pub(crate) fn activity(&self, id: &str) -> Vec<u8> {
        let mut body: Value = serde_json::from_slice(&self.ready.activity).unwrap();
        body["id"] = Value::String(id.to_owned());
        serde_json::to_vec(&body).unwrap()
    }
    pub(crate) async fn ingress(&self, body: &[u8]) -> reqwest::StatusCode {
        self.ingress_with_authorization(&self.ready.authorization, body)
            .await
    }
    pub(crate) async fn ingress_with_authorization(
        &self,
        authorization: &str,
        body: &[u8],
    ) -> reqwest::StatusCode {
        self.client
            .post(&self.ready.ingress_url)
            .header("authorization", authorization)
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
    }
    pub(crate) async fn snapshot(&self) -> Snapshot {
        self.client
            .post(format!("{}/snapshot", self.ready.control_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    pub(crate) async fn reply(&self, handle: &ReplyHandle, text: &str) -> ReplyResult {
        self.client
            .post(format!("{}/reply", self.ready.control_url))
            .json(&ReplyRequest {
                reply_handle: handle,
                text,
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    pub(crate) async fn begin(&self, id: u64, body: &[u8]) {
        self.client
            .post(format!("{}/begin", self.ready.control_url))
            .json(&BeginAdmission {
                operation_id: id,
                authorization: &self.ready.authorization,
                activity: body,
                now: self.ready.now,
            })
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    pub(crate) async fn wait(&self, id: u64) -> AdmissionResult {
        self.client
            .post(format!("{}/wait", self.ready.control_url))
            .json(&id)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }
    pub(crate) async fn cancel(&self, id: u64) {
        let cancelled: bool = self
            .client
            .post(format!("{}/cancel", self.ready.control_url))
            .json(&id)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(cancelled);
    }
    pub(crate) async fn close(&self) {
        self.client
            .post(format!("{}/close", self.ready.control_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        *self.callback.delivery.write().await = None;
    }
    pub(crate) async fn reopen(&self, delivery: TeamsDelivery) {
        *self.callback.delivery.write().await = Some(delivery);
        self.client
            .post(format!("{}/reopen", self.ready.control_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    pub(crate) fn join_panic_count(&self) -> usize {
        self.callback.join_panics.load(Ordering::SeqCst)
    }
    pub(crate) fn owner_join_panic_count(&self) -> usize {
        self.callback.owner_join_panics.load(Ordering::SeqCst)
    }
    pub(crate) fn owner_operation_id(&self) -> u64 {
        self.callback.owner_operation.load(Ordering::SeqCst)
    }
    pub(crate) async fn shutdown(&mut self) {
        self.client
            .post(format!("{}/shutdown", self.ready.control_url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(status) = self.child.0.try_wait().unwrap() {
                    break status;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(status.success());
        self.callback_task.0.abort();
    }
}
