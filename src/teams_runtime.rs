//! Teams ingress bridge into Dione's existing resident delivery machinery.

use crate::{
    codex::{CodexEventQueue, LiveEnqueueReceipt, ProviderEventKey},
    teams::AuthenticatedTeamsEnvelope,
    teams_edge::{BotTokenProvider, LiveReplyReceipt, TeamsEdge, TeamsEdgeError},
    teams_reply_store::{
        AsyncTeamsReplyStore, ReplyStoreError, SealedReplyAuthority, StoreReceipt, TeamsEventKey,
    },
    timestamp::Timestamp,
};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

const DEFAULT_PENDING_LIMIT: usize = 256;
const REPLY_HANDLE_TTL: Duration = Duration::from_secs(15 * 60);

struct PendingReply {
    envelope: AuthenticatedTeamsEnvelope,
    expires_at: Instant,
}

#[derive(Clone)]
/// Owns authenticated Teams ingress, resident notification, and single-use reply authority.
pub struct TeamsResidentBridge<P> {
    edge: Arc<TeamsEdge<P>>,
    queue: CodexEventQueue,
    pending: Arc<Mutex<HashMap<String, PendingReply>>>,
    pending_limit: usize,
    durable: Option<AsyncTeamsReplyStore>,
    admission: Arc<tokio::sync::Mutex<()>>,
    reply_tasks: TaskTracker,
}

impl<P: BotTokenProvider + 'static> TeamsResidentBridge<P> {
    /// Construct an in-memory bridge for a resident queue.
    pub fn new(edge: TeamsEdge<P>, queue: CodexEventQueue) -> Self {
        Self {
            edge: Arc::new(edge),
            queue,
            pending: Arc::new(Mutex::new(HashMap::new())),
            pending_limit: DEFAULT_PENDING_LIMIT,
            durable: None,
            admission: Arc::new(tokio::sync::Mutex::new(())),
            reply_tasks: TaskTracker::new(),
        }
    }

    /// Restore reply authority before accepting new resident notifications.
    pub async fn new_durable(
        edge: TeamsEdge<P>,
        queue: CodexEventQueue,
        state_dir: &camino::Utf8Path,
    ) -> Result<Self, TeamsRuntimeError> {
        let store = AsyncTeamsReplyStore::load(state_dir, edge.admission_policy().clone()).await?;
        let mut bridge = Self::new(edge, queue);
        bridge.durable = Some(store);
        Ok(bridge)
    }

    #[cfg(test)]
    pub(crate) fn poison_pending_for_test(&self) {
        let pending = self.pending.clone();
        let result = std::thread::spawn(move || {
            let _guard = pending.lock().expect("test registry starts healthy");
            panic!("deliberately poison reply registry");
        })
        .join();
        assert!(result.is_err(), "test thread must poison the registry");
    }

    /// Authenticate a Teams Activity, retain its sealed reply authority, and
    /// enqueue a transport-specific notification for the already-bound Codex resident.
    pub async fn admit(
        &self,
        authorization: &str,
        raw_activity: &[u8],
        now: u64,
    ) -> Result<(), TeamsRuntimeError> {
        let envelope = self
            .edge
            .authenticate(authorization, raw_activity, now)
            .await?;
        let _admission = self.admission.lock().await;
        let key = TeamsEventKey::from_envelope(&envelope);
        let provider_key = ProviderEventKey::teams(
            &key.tenant_id,
            &key.bot_id,
            &key.conversation_id,
            &key.activity_id,
        )
        .map_err(|error| TeamsRuntimeError::Resident(error.to_string()))?;
        if let Some(store) = &self.durable {
            let now = chrono::Utc::now();
            let (handle, created) =
                if let Some(handle) = store.find_handle(key.clone(), now).await? {
                    (handle, false)
                } else {
                    let handle = format!("teams-{}", Uuid::new_v4());
                    let authority = SealedReplyAuthority::seal(
                        &envelope,
                        &self.edge.admission_policy().app_id,
                        now + chrono::TimeDelta::minutes(15),
                    );
                    match store.insert(handle.clone(), authority, now).await? {
                        StoreReceipt::Persisted(()) => (handle, true),
                        StoreReceipt::VisibleDurabilityUncertain((), error) => {
                            return Err(ReplyStoreError::DurabilityUncertain(error).into());
                        }
                    }
                };
            let notification = resident_notification(&envelope, &handle);
            return match self.queue.enqueue_live(notification, provider_key).await {
                Ok(LiveEnqueueReceipt::Committed) => Ok(()),
                Ok(LiveEnqueueReceipt::CommittedDurabilityUncertain(error)) => {
                    Err(TeamsRuntimeError::Resident(error.to_string()))
                }
                Ok(LiveEnqueueReceipt::Duplicate) => {
                    if created {
                        match store.discard(handle.clone(), now).await? {
                            StoreReceipt::Persisted(()) => {}
                            StoreReceipt::VisibleDurabilityUncertain((), error) => {
                                return Err(ReplyStoreError::DurabilityUncertain(error).into());
                            }
                        }
                    }
                    Ok(())
                }
                Err(error) => {
                    if created {
                        match store.discard(handle.clone(), now).await? {
                            StoreReceipt::Persisted(()) => {}
                            StoreReceipt::VisibleDurabilityUncertain((), source) => {
                                return Err(ReplyStoreError::DurabilityUncertain(source).into());
                            }
                        }
                    }
                    Err(TeamsRuntimeError::Resident(error.to_string()))
                }
            };
        }
        let handle = format!("teams-{}", Uuid::new_v4());
        let notification = resident_notification(&envelope, &handle);
        {
            let mut pending = self.pending_replies()?;
            pending.retain(|_, reply| reply.expires_at > Instant::now());
            if pending.len() >= self.pending_limit {
                return Err(TeamsRuntimeError::ReplyRegistryFull);
            }
            pending.insert(
                handle.clone(),
                PendingReply {
                    envelope,
                    expires_at: Instant::now() + REPLY_HANDLE_TTL,
                },
            );
        }
        match self.queue.enqueue_live(notification, provider_key).await {
            Ok(LiveEnqueueReceipt::Committed) => Ok(()),
            Ok(LiveEnqueueReceipt::CommittedDurabilityUncertain(error)) => {
                Err(TeamsRuntimeError::Resident(error.to_string()))
            }
            Ok(LiveEnqueueReceipt::Duplicate) => {
                self.pending_replies()?.remove(&handle);
                Ok(())
            }
            Err(error) => {
                self.pending_replies()?.remove(&handle);
                Err(TeamsRuntimeError::Resident(error.to_string()))
            }
        }
    }

    /// Consume a reply handle once, retaining an accepted outbound send after caller cancellation.
    pub async fn reply(
        &self,
        handle: &str,
        text: &str,
    ) -> Result<LiveReplyReceipt, TeamsRuntimeError> {
        if let Some(store) = &self.durable {
            let authority = store.get(handle.to_owned(), chrono::Utc::now()).await?;
            let envelope = authority.open(self.edge.admission_policy())?;
            let prepared = self.edge.prepare_reply(&envelope, text).await?;
            let store = store.clone();
            let edge = self.edge.clone();
            let handle = handle.to_owned();
            let task = self.reply_tasks.spawn(async move {
                let result = async {
                    match store.take(handle, chrono::Utc::now()).await? {
                        StoreReceipt::Persisted(_) => {}
                        StoreReceipt::VisibleDurabilityUncertain(_, error) => {
                            return Err(ReplyStoreError::DurabilityUncertain(error).into());
                        }
                    }
                    edge.send_prepared_reply(prepared)
                        .await
                        .map_err(TeamsRuntimeError::ReplyOutcomeUncertain)
                }
                .await;
                if let Err(error) = &result {
                    tracing::error!(%error, "Teams owned reply task failed");
                }
                result
            });
            return task
                .await
                .map_err(|error| TeamsRuntimeError::ReplyTask(error.to_string()))?;
        }
        let envelope = {
            let mut replies = self.pending_replies()?;
            replies.retain(|_, reply| reply.expires_at > Instant::now());
            replies
                .get(handle)
                .ok_or(TeamsRuntimeError::UnknownReplyHandle)?
                .envelope
                .clone()
        };
        let prepared = self.edge.prepare_reply(&envelope, text).await?;
        {
            let mut replies = self.pending_replies()?;
            replies.retain(|_, reply| reply.expires_at > Instant::now());
            replies
                .remove(handle)
                .ok_or(TeamsRuntimeError::UnknownReplyHandle)?;
        }
        let edge = self.edge.clone();
        let task = self.reply_tasks.spawn(async move {
            let result = edge
                .send_prepared_reply(prepared)
                .await
                .map_err(TeamsRuntimeError::ReplyOutcomeUncertain);
            if let Err(error) = &result {
                tracing::error!(%error, "Teams owned reply task failed");
            }
            result
        });
        task.await
            .map_err(|error| TeamsRuntimeError::ReplyTask(error.to_string()))?
    }

    fn pending_replies(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, PendingReply>>, TeamsRuntimeError> {
        self.pending.lock().map_err(|_| {
            TeamsRuntimeError::ReplyAuthority("reply registry lock poisoned".to_owned())
        })
    }

    /// Wait for sends that already consumed a reply handle, including sends
    /// whose MCP caller disconnected. Call only after new tool calls quiesce.
    pub async fn drain_replies(&self) {
        self.reply_tasks.close();
        self.reply_tasks.wait().await;
    }
}

/// Serve authenticated Teams Activities until cancellation closes the listener.
pub async fn run_listener<P: BotTokenProvider + Clone + 'static>(
    bridge: Arc<TeamsResidentBridge<P>>,
    listen: std::net::SocketAddr,
    cancel: CancellationToken,
) -> Result<(), std::io::Error> {
    let app = Router::new()
        .route("/api/messages", post(messages::<P>))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(bridge);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let bound = listener.local_addr()?;
    tracing::info!(listen = %bound, "Teams resident listener bound");
    let shutdown = async move {
        cancel.cancelled_owned().await;
        tracing::info!(listen = %bound, "Teams resident listener received cancellation");
    };
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    match &result {
        Ok(()) => tracing::info!(listen = %bound, "Teams resident listener stopped normally"),
        Err(error) => tracing::error!(listen = %bound, %error, "Teams resident listener failed"),
    }
    result
}

pub(crate) async fn messages<P: BotTokenProvider + 'static>(
    State(bridge): State<Arc<TeamsResidentBridge<P>>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(authorization) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    match bridge.admit(authorization, &body, now.as_secs()).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(TeamsRuntimeError::Edge(TeamsEdgeError::Probe(_))) => StatusCode::UNAUTHORIZED,
        Err(error) => {
            tracing::error!(%error, "Teams resident ingress failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn resident_notification(envelope: &AuthenticatedTeamsEnvelope, reply_handle: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": envelope.text,
            "meta": {
                "provider": "teams",
                "message_id": envelope.reference.incoming_activity_id,
                "conversation_id": envelope.reference.conversation_id,
                "channel_id": envelope.reference.channel_id,
                "user_id": envelope.reference.sender_id,
                "bot_id": envelope.reference.bot_id,
                "tenant_id": envelope.reference.dione_authorized_tenant_id,
                "reply_handle": reply_handle,
                "ts": Timestamp::now(),
            }
        }
    })
}

/// Consume opaque reply handles without exposing connector credentials to MCP callers.
pub trait TeamsReplyAuthority: Send + Sync {
    /// Send one reply for a previously authenticated Activity.
    fn reply<'a>(
        &'a self,
        handle: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReplyReceipt, TeamsRuntimeError>> + Send + 'a>>;
}

impl<P: BotTokenProvider + 'static> TeamsReplyAuthority for TeamsResidentBridge<P> {
    fn reply<'a>(
        &'a self,
        handle: &'a str,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReplyReceipt, TeamsRuntimeError>> + Send + 'a>>
    {
        Box::pin(async move { self.reply(handle, text).await })
    }
}

#[derive(Debug, Error)]
/// Failure to authenticate, enqueue, or reply to a Teams Activity.
pub enum TeamsRuntimeError {
    #[error(transparent)]
    Edge(#[from] TeamsEdgeError),
    #[error("teams reply authority failed: {0}")]
    ReplyAuthority(String),
    #[error("teams reply task failed: {0}")]
    ReplyTask(String),
    #[error("teams reply registry is full")]
    ReplyRegistryFull,
    #[error("teams reply handle is unknown or already consumed")]
    UnknownReplyHandle,
    #[error("teams reply outcome is uncertain after outbound send was attempted: {0}")]
    ReplyOutcomeUncertain(TeamsEdgeError),
    #[error("resident delivery failed: {0}")]
    Resident(String),
}

impl From<ReplyStoreError> for TeamsRuntimeError {
    fn from(error: ReplyStoreError) -> Self {
        match error {
            ReplyStoreError::UnknownHandle => Self::UnknownReplyHandle,
            other => Self::ReplyAuthority(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        teams::{AdmissionPolicy, AuthenticatedConversationReference},
        teams_edge::{BotAccessToken, TeamsEdgeError},
    };
    use std::{
        collections::BTreeSet,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tempfile::TempDir;

    #[test]
    fn teams_error_display_fragments_start_lowercase() {
        assert_eq!(
            TeamsRuntimeError::ReplyRegistryFull.to_string(),
            "teams reply registry is full"
        );
        assert_eq!(
            ReplyStoreError::Full.to_string(),
            "teams reply registry is full"
        );
        assert_eq!(
            TeamsEdgeError::MetadataTooLarge.to_string(),
            "teams metadata response exceeds its size limit"
        );
        assert_eq!(
            crate::codex::LiveQueueError::ReplayCapacity.to_string(),
            "teams replay-identity capacity is full"
        );
    }

    #[derive(Clone)]
    struct UnusedTokenProvider;

    impl BotTokenProvider for UnusedTokenProvider {
        async fn access_token(&self) -> Result<BotAccessToken, TeamsEdgeError> {
            panic!("listener bind test must not acquire a token")
        }
    }

    #[derive(Clone)]
    struct FailingTokenProvider(Arc<AtomicUsize>);

    impl BotTokenProvider for FailingTokenProvider {
        async fn access_token(&self) -> Result<BotAccessToken, TeamsEdgeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(TeamsEdgeError::InvalidTokenEndpoint)
        }
    }

    #[derive(Clone)]
    struct GatedTokenProvider {
        attempts: Arc<AtomicUsize>,
        started: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl BotTokenProvider for GatedTokenProvider {
        async fn access_token(&self) -> Result<BotAccessToken, TeamsEdgeError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            self.release.notified().await;
            Err(TeamsEdgeError::InvalidTokenEndpoint)
        }
    }

    #[tokio::test]
    async fn cancellation_during_token_acquisition_keeps_reply_handle_retryable() {
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(&queue_path).expect("load queue");
        let attempts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            GatedTokenProvider {
                attempts: attempts.clone(),
                started: started.clone(),
                release: release.clone(),
            },
            AdmissionPolicy {
                app_id: "app".to_owned(),
                tenant_id: "tenant".to_owned(),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let bridge = Arc::new(TeamsResidentBridge::new(edge, queue));
        bridge.pending.lock().expect("Teams reply registry").insert(
            "cancelled-handle".to_owned(),
            PendingReply {
                envelope: AuthenticatedTeamsEnvelope {
                    reference: AuthenticatedConversationReference {
                        service_url: "https://connector.test/".parse().unwrap(),
                        channel_id: "msteams".to_owned(),
                        conversation_id: "conversation".to_owned(),
                        incoming_activity_id: "activity".to_owned(),
                        bot_id: "bot".to_owned(),
                        sender_id: "sender".to_owned(),
                        dione_authorized_tenant_id: "tenant".to_owned(),
                    },
                    text: "hello".to_owned(),
                },
                expires_at: Instant::now() + REPLY_HANDLE_TTL,
            },
        );

        let in_flight = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.reply("cancelled-handle", "reply").await }
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("first token acquisition started");
        in_flight.abort();
        assert!(in_flight.await.expect_err("task aborted").is_cancelled());

        release.notify_one();
        assert!(matches!(
            bridge.reply("cancelled-handle", "reply").await,
            Err(TeamsRuntimeError::Edge(
                TeamsEdgeError::InvalidTokenEndpoint
            ))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn token_failure_before_send_keeps_reply_handle_retryable() {
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(&queue_path).expect("load queue");
        let attempts = Arc::new(AtomicUsize::new(0));
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            FailingTokenProvider(attempts.clone()),
            AdmissionPolicy {
                app_id: "app".to_owned(),
                tenant_id: "tenant".to_owned(),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let bridge = TeamsResidentBridge::new(edge, queue);
        bridge.pending.lock().expect("Teams reply registry").insert(
            "retryable-handle".to_owned(),
            PendingReply {
                envelope: AuthenticatedTeamsEnvelope {
                    reference: AuthenticatedConversationReference {
                        service_url: "https://connector.test/".parse().unwrap(),
                        channel_id: "msteams".to_owned(),
                        conversation_id: "conversation".to_owned(),
                        incoming_activity_id: "activity".to_owned(),
                        bot_id: "bot".to_owned(),
                        sender_id: "sender".to_owned(),
                        dione_authorized_tenant_id: "tenant".to_owned(),
                    },
                    text: "hello".to_owned(),
                },
                expires_at: Instant::now() + REPLY_HANDLE_TTL,
            },
        );

        assert!(matches!(
            bridge.reply("retryable-handle", "reply").await,
            Err(TeamsRuntimeError::Edge(
                TeamsEdgeError::InvalidTokenEndpoint
            ))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let retry = bridge
            .reply("retryable-handle", "reply")
            .await
            .expect_err("token acquisition also fails on a safe retry");
        assert!(
            matches!(
                &retry,
                TeamsRuntimeError::Edge(TeamsEdgeError::InvalidTokenEndpoint)
            ),
            "pre-send failure must retain handle, got {retry:?}"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn durable_handle_survives_pre_send_failure_and_restart() {
        let temp = TempDir::new().expect("create queue directory");
        let state_dir =
            camino::Utf8PathBuf::from_path_buf(temp.path().to_owned()).expect("UTF-8 state path");
        let queue = CodexEventQueue::load(&state_dir).expect("load queue");
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut policy = AdmissionPolicy {
            app_id: "app".to_owned(),
            tenant_id: "tenant".to_owned(),
            allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
            channels: Default::default(),
        };
        policy.channels.insert(
            "msteams".to_owned(),
            crate::teams::ChannelPolicy {
                requires_key_endorsement: true,
            },
        );
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            FailingTokenProvider(attempts.clone()),
            policy,
        )
        .expect("construct edge");
        let envelope = AuthenticatedTeamsEnvelope {
            reference: AuthenticatedConversationReference {
                service_url: "https://connector.test/".parse().unwrap(),
                channel_id: "msteams".to_owned(),
                conversation_id: "conversation".to_owned(),
                incoming_activity_id: "activity".to_owned(),
                bot_id: "bot".to_owned(),
                sender_id: "sender".to_owned(),
                dione_authorized_tenant_id: "tenant".to_owned(),
            },
            text: "not stored".to_owned(),
        };
        let handle = format!("teams-{}", Uuid::new_v4());
        let now = chrono::Utc::now();
        let bridge = TeamsResidentBridge::new_durable(edge.clone(), queue.clone(), &state_dir)
            .await
            .expect("load durable bridge");
        let receipt = bridge
            .durable
            .as_ref()
            .expect("durable store")
            .insert(
                handle.clone(),
                SealedReplyAuthority::seal(&envelope, "app", now + chrono::TimeDelta::minutes(15)),
                now,
            )
            .await
            .expect("persist handle");
        assert!(matches!(receipt, StoreReceipt::Persisted(())));
        assert!(matches!(
            bridge.reply(&handle, "reply").await,
            Err(TeamsRuntimeError::Edge(
                TeamsEdgeError::InvalidTokenEndpoint
            ))
        ));
        drop(bridge);
        let restored = TeamsResidentBridge::new_durable(edge, queue, &state_dir)
            .await
            .expect("restore handle after token failure");
        assert!(matches!(
            restored.reply(&handle, "reply").await,
            Err(TeamsRuntimeError::Edge(
                TeamsEdgeError::InvalidTokenEndpoint
            ))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn teams_listener_bind_failure_does_not_cancel_discord_lifecycle() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy local port");
        let listen = occupied.local_addr().expect("occupied address");
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(&queue_path).expect("load queue");
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            UnusedTokenProvider,
            AdmissionPolicy {
                app_id: "app".to_owned(),
                tenant_id: "tenant".to_owned(),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let lifecycle = CancellationToken::new();
        let error = run_listener(
            Arc::new(TeamsResidentBridge::new(edge, queue)),
            listen,
            lifecycle.clone(),
        )
        .await
        .expect_err("occupied port rejects Teams listener");
        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
        assert!(!lifecycle.is_cancelled());
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(!lifecycle.is_cancelled());
    }

    #[tokio::test]
    async fn teams_listener_remains_alive_idle_until_explicit_cancellation() {
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve local port");
        let listen = reserved.local_addr().expect("reserved address");
        drop(reserved);
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(&queue_path).expect("load queue");
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            UnusedTokenProvider,
            AdmissionPolicy {
                app_id: "app".to_owned(),
                tenant_id: "tenant".to_owned(),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let lifecycle = CancellationToken::new();
        let task_lifecycle = lifecycle.clone();
        let mut listener = tokio::spawn(run_listener(
            Arc::new(TeamsResidentBridge::new(edge, queue)),
            listen,
            task_lifecycle,
        ));

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if tokio::net::TcpStream::connect(listen).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("listener binds");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut listener)
                .await
                .is_err(),
            "idle listener exited without cancellation"
        );

        lifecycle.cancel();
        tokio::time::timeout(Duration::from_secs(1), listener)
            .await
            .expect("listener stops after cancellation")
            .expect("listener task joins")
            .expect("listener stops normally");
    }
}
