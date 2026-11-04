//! Teams ingress bridge into Dione's existing resident delivery machinery.

use crate::{
    delivery::ProviderEventKey,
    teams::{
        AuthenticatedTeamsEnvelope, ReplyHandle,
        delivery::{TeamsAdmissionOutcome, TeamsDelivery, TeamsDeliveryError, TeamsResidentEvent},
        edge::{BotTokenProvider, LiveReplyReceipt, TeamsEdge, TeamsEdgeError},
        reply_store::{
            AsyncTeamsReplyStore, ReplyStoreError, ReplyStoreErrorKind, SealedReplyAuthority,
            StoreReceipt, TeamsEventKey,
        },
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
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

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
    delivery: TeamsDelivery,
    pending: Arc<Mutex<HashMap<ReplyHandle, PendingReply>>>,
    pending_limit: usize,
    durable: Option<AsyncTeamsReplyStore>,
    admission: Arc<tokio::sync::Mutex<()>>,
    reply_tasks: TaskTracker,
}

impl<P: BotTokenProvider + 'static> TeamsResidentBridge<P> {
    /// Construct an in-memory bridge for resident delivery.
    pub fn new(edge: TeamsEdge<P>, delivery: TeamsDelivery) -> Self {
        Self {
            edge: Arc::new(edge),
            delivery,
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
        delivery: TeamsDelivery,
        state_dir: &camino::Utf8Path,
    ) -> Result<Self, TeamsRuntimeError> {
        let store = AsyncTeamsReplyStore::load(state_dir, edge.admission_policy().clone()).await?;
        let mut bridge = Self::new(edge, delivery);
        bridge.durable = Some(store);
        Ok(bridge)
    }

    #[cfg(test)]
    pub(super) fn authority_store_for_test(&self) -> AsyncTeamsReplyStore {
        self.durable.as_ref().expect("durable test bridge").clone()
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
    /// deliver it to the resident.
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
            key.tenant_id.clone(),
            key.bot_id.clone(),
            key.conversation_id.clone(),
            key.activity_id.clone(),
        );
        if let Some(store) = &self.durable {
            let now = chrono::Utc::now();
            let (handle, created) =
                if let Some(handle) = store.find_handle(key.clone(), now).await? {
                    (handle, false)
                } else {
                    let handle = ReplyHandle::generate();
                    let authority = SealedReplyAuthority::seal(
                        &envelope,
                        &self.edge.admission_policy().app_id,
                        now + chrono::TimeDelta::minutes(15),
                    );
                    match store.insert(handle.clone(), authority, now).await? {
                        StoreReceipt::Persisted(()) => (handle, true),
                        StoreReceipt::VisibleDurabilityUncertain((), error) => {
                            return Err(TeamsRuntimeError {
                                kind: TeamsRuntimeErrorKind::AuthorityCommittedUncertain {
                                    operation: ReplyAuthorityOperation::Mint,
                                    source: error,
                                },
                            });
                        }
                    }
                };
            let event = resident_event(&envelope, &handle, provider_key);
            return match self.delivery.deliver(event).await {
                Ok(TeamsAdmissionOutcome::Committed) => Ok(()),
                Ok(TeamsAdmissionOutcome::CommittedDurabilityUncertain(error)) => {
                    Err(TeamsRuntimeError {
                        kind: TeamsRuntimeErrorKind::ResidentCommittedUncertain(error),
                    })
                }
                Ok(TeamsAdmissionOutcome::CompletionUnknown(error)) => Err(TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::ResidentCompletionUnknown(error),
                }),
                Ok(TeamsAdmissionOutcome::Duplicate) => {
                    if created {
                        match store.discard(handle.clone(), now).await? {
                            StoreReceipt::Persisted(()) => {}
                            StoreReceipt::VisibleDurabilityUncertain((), error) => {
                                return Err(TeamsRuntimeError {
                                    kind: TeamsRuntimeErrorKind::AuthorityCommittedUncertain {
                                        operation: ReplyAuthorityOperation::Discard,
                                        source: error,
                                    },
                                });
                            }
                        }
                    }
                    Ok(())
                }
                Err(error) => {
                    if created {
                        let cleanup = match store.discard(handle.clone(), now).await {
                            Ok(StoreReceipt::Persisted(())) => None,
                            Ok(StoreReceipt::VisibleDurabilityUncertain((), source)) => {
                                Some(AuthorityCleanupError {
                                    kind: AuthorityCleanupErrorKind::CommittedDurabilityUncertain(
                                        source,
                                    ),
                                })
                            }
                            Err(source) => Some(AuthorityCleanupError {
                                kind: AuthorityCleanupErrorKind::Rejected(source),
                            }),
                        };
                        if let Some(cleanup) = cleanup {
                            return Err(TeamsRuntimeError {
                                kind: TeamsRuntimeErrorKind::AdmissionCleanup {
                                    delivery: error,
                                    cleanup,
                                },
                            });
                        }
                    }
                    Err(TeamsRuntimeError {
                        kind: TeamsRuntimeErrorKind::Resident(error),
                    })
                }
            };
        }
        let handle = ReplyHandle::generate();
        let event = resident_event(&envelope, &handle, provider_key);
        {
            let mut pending = self.pending_replies()?;
            pending.retain(|_, reply| reply.expires_at > Instant::now());
            if pending.len() >= self.pending_limit {
                return Err(TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::ReplyRegistryFull,
                });
            }
            pending.insert(
                handle.clone(),
                PendingReply {
                    envelope,
                    expires_at: Instant::now() + REPLY_HANDLE_TTL,
                },
            );
        }
        match self.delivery.deliver(event).await {
            Ok(TeamsAdmissionOutcome::Committed) => Ok(()),
            Ok(TeamsAdmissionOutcome::CommittedDurabilityUncertain(error)) => {
                Err(TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::ResidentCommittedUncertain(error),
                })
            }
            Ok(TeamsAdmissionOutcome::CompletionUnknown(error)) => Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::ResidentCompletionUnknown(error),
            }),
            Ok(TeamsAdmissionOutcome::Duplicate) => {
                self.pending_replies()?.remove(&handle);
                Ok(())
            }
            Err(error) => {
                match self.pending_replies() {
                    Ok(mut pending) => {
                        pending.remove(&handle);
                    }
                    Err(_) => {
                        return Err(TeamsRuntimeError {
                            kind: TeamsRuntimeErrorKind::AdmissionRegistryCleanup(error),
                        });
                    }
                }
                Err(TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::Resident(error),
                })
            }
        }
    }

    /// Consume a reply handle once, retaining an accepted outbound send after caller cancellation.
    pub async fn reply(
        &self,
        handle: &ReplyHandle,
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
                            return Err(TeamsRuntimeError {
                                kind: TeamsRuntimeErrorKind::AuthorityCommittedUncertain {
                                    operation: ReplyAuthorityOperation::Consume,
                                    source: error,
                                },
                            });
                        }
                    }
                    edge.send_prepared_reply(prepared)
                        .await
                        .map_err(|source| TeamsRuntimeError {
                            kind: TeamsRuntimeErrorKind::ReplyOutcomeUncertain(source),
                        })
                }
                .await;
                if let Err(error) = &result {
                    tracing::error!(%error, "Teams owned reply task failed");
                }
                result
            });
            return settle_reply(task).await;
        }
        let envelope = {
            let mut replies = self.pending_replies()?;
            replies.retain(|_, reply| reply.expires_at > Instant::now());
            replies
                .get(handle)
                .ok_or(TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::UnknownReplyHandle,
                })?
                .envelope
                .clone()
        };
        let prepared = self.edge.prepare_reply(&envelope, text).await?;
        {
            let mut replies = self.pending_replies()?;
            replies.retain(|_, reply| reply.expires_at > Instant::now());
            replies.remove(handle).ok_or(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::UnknownReplyHandle,
            })?;
        }
        let edge = self.edge.clone();
        let task = self.reply_tasks.spawn(async move {
            let result =
                edge.send_prepared_reply(prepared)
                    .await
                    .map_err(|source| TeamsRuntimeError {
                        kind: TeamsRuntimeErrorKind::ReplyOutcomeUncertain(source),
                    });
            if let Err(error) = &result {
                tracing::error!(%error, "Teams owned reply task failed");
            }
            result
        });
        settle_reply(task).await
    }

    fn pending_replies(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<ReplyHandle, PendingReply>>, TeamsRuntimeError>
    {
        self.pending.lock().map_err(|_| TeamsRuntimeError {
            kind: TeamsRuntimeErrorKind::RegistryPoisoned,
        })
    }

    /// Wait for sends that already consumed a reply handle, including sends
    /// whose MCP caller disconnected. Call only after new tool calls quiesce.
    pub async fn drain_replies(&self) {
        self.reply_tasks.close();
        self.reply_tasks.wait().await;
    }
}

async fn settle_reply(
    task: tokio::task::JoinHandle<Result<LiveReplyReceipt, TeamsRuntimeError>>,
) -> Result<LiveReplyReceipt, TeamsRuntimeError> {
    task.await.map_err(|source| TeamsRuntimeError {
        kind: TeamsRuntimeErrorKind::ReplyTask(source),
    })?
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
        let reason = if headers.contains_key("authorization") {
            "invalid_authorization_header"
        } else {
            "missing_authorization_header"
        };
        log_ingress_rejection(
            &headers,
            reason,
            "authorization_header",
            "check that IIS and intermediate proxies forward a valid Authorization header unchanged",
        );
        return StatusCode::UNAUTHORIZED;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    match bridge.admit(authorization, &body, now.as_secs()).await {
        Ok(()) => StatusCode::ACCEPTED,
        Err(TeamsRuntimeError {
            kind:
                TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: crate::teams::TeamsEdgeErrorKind::Probe(error),
                }),
        }) => {
            log_ingress_rejection(
                &headers,
                error.rejection_code(),
                error.rejection_stage(),
                error.rejection_guidance(),
            );
            StatusCode::UNAUTHORIZED
        }
        Err(error) => {
            tracing::error!(%error, "Teams resident ingress failed");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn log_ingress_rejection(
    headers: &HeaderMap,
    reason: &'static str,
    stage: &'static str,
    remediation: &'static str,
) {
    let arr_request_id = headers
        .get("x-arr-log-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| uuid::Uuid::parse_str(value).ok());
    tracing::warn!(
        component = "teams_ingress",
        ?arr_request_id,
        reason,
        stage,
        remediation,
        "Teams ingress rejected"
    );
}

fn resident_event(
    envelope: &AuthenticatedTeamsEnvelope,
    reply_handle: &ReplyHandle,
    provider_key: ProviderEventKey,
) -> TeamsResidentEvent {
    TeamsResidentEvent {
        provider_key,
        content: envelope.text.clone(),
        message_id: envelope.reference.incoming_activity_id.clone(),
        conversation_id: envelope.reference.conversation_id.clone(),
        channel_id: envelope.reference.channel_id.clone(),
        user_id: envelope.reference.sender_id.clone(),
        bot_id: envelope.reference.bot_id.clone(),
        tenant_id: envelope.reference.dione_authorized_tenant_id.clone(),
        reply_handle: reply_handle.to_owned(),
        timestamp: Timestamp::now(),
    }
}

/// Consume opaque reply handles without exposing connector credentials to MCP callers.
pub trait TeamsReplyAuthority: Send + Sync {
    /// Send one reply for a previously authenticated Activity.
    fn reply<'a>(
        &'a self,
        handle: &'a ReplyHandle,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReplyReceipt, TeamsRuntimeError>> + Send + 'a>>;
}

impl<P: BotTokenProvider + 'static> TeamsReplyAuthority for TeamsResidentBridge<P> {
    fn reply<'a>(
        &'a self,
        handle: &'a ReplyHandle,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<LiveReplyReceipt, TeamsRuntimeError>> + Send + 'a>>
    {
        Box::pin(async move { self.reply(handle, text).await })
    }
}

/// The authority mutation whose visible result needs a durability barrier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplyAuthorityOperation {
    /// Retaining authority for newly admitted work.
    Mint,
    /// Removing unused authority after duplicate admission.
    Discard,
    /// Removing single-use authority before an outbound send.
    Consume,
}

/// A failed authority cleanup after resident admission was rejected.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct AuthorityCleanupError {
    /// Whether cleanup itself committed and its original cause.
    #[source]
    pub kind: AuthorityCleanupErrorKind,
}

/// Authority cleanup preserves the phase of its own failed mutation.
#[derive(Debug, Error)]
pub enum AuthorityCleanupErrorKind {
    /// Cleanup did not commit; prior authority remains visible.
    #[error("authority cleanup rejected before commit")]
    Rejected(#[source] ReplyStoreError),
    /// Cleanup removed authority but its directory sync failed.
    #[error("authority cleanup committed with uncertain durability")]
    CommittedDurabilityUncertain(#[source] std::io::Error),
}

/// A failed Teams admission or reply operation.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct TeamsRuntimeError {
    /// The semantic classification and retained original cause.
    #[source]
    pub kind: TeamsRuntimeErrorKind,
}

impl From<TeamsRuntimeErrorKind> for TeamsRuntimeError {
    fn from(kind: TeamsRuntimeErrorKind) -> Self {
        Self { kind }
    }
}
impl From<TeamsEdgeError> for TeamsRuntimeError {
    fn from(source: TeamsEdgeError) -> Self {
        Self {
            kind: source.into(),
        }
    }
}

#[derive(Debug, Error)]
/// Failure to authenticate, enqueue, or reply to a Teams Activity.
pub enum TeamsRuntimeErrorKind {
    /// An owned authority task lost its result; mutation completion must not be inferred.
    #[error("reply authority operation completion is unknown")]
    AuthorityCompletionUnknown(#[source] ReplyStoreError),
    /// Admission ownership transferred, but completion was lost. Reply authority remains.
    #[error("resident admission completion is unknown")]
    ResidentCompletionUnknown(#[source] TeamsDeliveryError),
    /// Authority changed visibly, but its crash durability remains uncertain.
    #[error("reply authority {operation:?} committed with uncertain durability")]
    AuthorityCommittedUncertain {
        /// The authority mutation that committed.
        operation: ReplyAuthorityOperation,
        /// The failed directory sync after the visible mutation.
        #[source]
        source: std::io::Error,
    },
    /// Delivery was rejected and cleanup could not lock the poisoned registry.
    #[error("resident delivery rejected and registry cleanup lock poisoned")]
    AdmissionRegistryCleanup(#[source] TeamsDeliveryError),
    /// The durable authority registry reached capacity.
    #[error("teams reply registry is full")]
    StoreCapacity(#[source] ReplyStoreError),
    /// The event committed but its directory sync failed.
    #[error("resident delivery committed with uncertain durability")]
    ResidentCommittedUncertain(#[source] TeamsDeliveryError),
    /// Admission failed and its newly created authority could not be removed.
    #[error("resident delivery rejected and authority cleanup failed")]
    AdmissionCleanup {
        /// The original precommit delivery rejection is the primary cause.
        #[source]
        delivery: TeamsDeliveryError,
        /// The independent failure while removing unused authority.
        cleanup: AuthorityCleanupError,
    },
    /// The in-memory authority registry was poisoned.
    #[error("reply registry lock poisoned")]
    RegistryPoisoned,
    /// Platform authentication, preparation, or reply delivery failed.
    #[error(transparent)]
    Edge(#[from] TeamsEdgeError),
    /// Teams reply authority failed.
    #[error("teams reply authority failed")]
    ReplyAuthority(#[source] ReplyStoreError),
    /// Teams reply task failed.
    #[error("teams reply task failed")]
    ReplyTask(#[source] tokio::task::JoinError),
    /// Teams reply registry is full.
    #[error("teams reply registry is full")]
    ReplyRegistryFull,
    /// Teams reply handle is unknown or already consumed.
    #[error("teams reply handle is unknown or already consumed")]
    UnknownReplyHandle,
    /// Teams reply outcome is uncertain after outbound send was attempted.
    #[error("teams reply outcome is uncertain after outbound send was attempted")]
    ReplyOutcomeUncertain(#[source] TeamsEdgeError),
    /// Resident delivery failed.
    #[error("resident delivery failed")]
    Resident(#[source] TeamsDeliveryError),
}

impl From<ReplyStoreError> for TeamsRuntimeError {
    fn from(error: ReplyStoreError) -> Self {
        match error {
            ReplyStoreError {
                kind: ReplyStoreErrorKind::UnknownHandle,
            } => Self {
                kind: TeamsRuntimeErrorKind::UnknownReplyHandle,
            },
            error @ ReplyStoreError {
                kind: ReplyStoreErrorKind::Full,
            } => Self {
                kind: TeamsRuntimeErrorKind::StoreCapacity(error),
            },
            error @ ReplyStoreError {
                kind: ReplyStoreErrorKind::Task(_),
            } => Self {
                kind: TeamsRuntimeErrorKind::AuthorityCompletionUnknown(error),
            },
            other @ ReplyStoreError {
                kind:
                    ReplyStoreErrorKind::ServiceUrl(_)
                    | ReplyStoreErrorKind::OffloadClosed(_)
                    | ReplyStoreErrorKind::LockPoisoned
                    | ReplyStoreErrorKind::Owned(_)
                    | ReplyStoreErrorKind::Io(_)
                    | ReplyStoreErrorKind::Json(_)
                    | ReplyStoreErrorKind::Encode(_)
                    | ReplyStoreErrorKind::DurabilityUncertain(_)
                    | ReplyStoreErrorKind::InvalidRecord(_),
            } => Self {
                kind: TeamsRuntimeErrorKind::ReplyAuthority(other),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codex::CodexEventQueue,
        teams::{
            AdmissionPolicy, AuthenticatedConversationReference, ProbeError, ProbeErrorKind,
            delivery::TeamsEventSink,
            edge::{BotAccessToken, TeamsEdgeError},
        },
    };
    use axum::{
        extract::Request,
        middleware::{Next, from_fn},
    };
    use std::{
        collections::BTreeSet,
        io::{self, Write},
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };
    use tempfile::TempDir;
    use tracing::instrument::WithSubscriber;

    #[derive(Clone)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_rejection(headers: &HeaderMap, error: ProbeError) -> String {
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturedLog(Arc::clone(&bytes));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_ingress_rejection(
                headers,
                error.rejection_code(),
                error.rejection_stage(),
                error.rejection_guidance(),
            );
        });
        let captured = bytes.lock().unwrap().clone();
        String::from_utf8(captured).unwrap()
    }

    #[test]
    fn ingress_rejection_log_correlates_arr_without_logging_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer private-token-sentinel".parse().unwrap(),
        );
        headers.insert(
            "x-arr-log-id",
            "f1170039-ed04-48ab-b9a1-f4ee8ed07377".parse().unwrap(),
        );
        let log = capture_rejection(
            &headers,
            ProbeErrorKind::InvalidToken(jsonwebtoken::errors::ErrorKind::InvalidAudience.into())
                .into(),
        );
        assert!(log.contains("Teams ingress rejected"), "{log}");
        assert!(
            log.contains("f1170039-ed04-48ab-b9a1-f4ee8ed07377"),
            "{log}"
        );
        assert!(log.contains("invalid_token_audience"), "{log}");
        assert!(log.contains("TEAMS_APP_ID"), "{log}");
        assert!(log.contains("connector_authentication"), "{log}");
        assert!(!log.contains("private-token-sentinel"), "{log}");
    }

    #[test]
    fn ingress_rejection_log_discards_untrusted_arr_and_error_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-arr-log-id", "private-header-sentinel".parse().unwrap());
        let log = capture_rejection(
            &headers,
            ProbeErrorKind::InvalidToken(
                jsonwebtoken::errors::ErrorKind::MissingRequiredClaim(
                    "private-error-sentinel".to_owned(),
                )
                .into(),
            )
            .into(),
        );
        assert!(log.contains("missing_token_claim"), "{log}");
        assert!(log.contains("arr_request_id=None"), "{log}");
        assert!(!log.contains("private-header-sentinel"), "{log}");
        assert!(!log.contains("private-error-sentinel"), "{log}");
    }

    struct RecordedDelivery(Arc<AtomicUsize>);

    impl TeamsEventSink for RecordedDelivery {
        fn deliver<'a>(
            &'a self,
            _event: TeamsResidentEvent,
        ) -> Pin<
            Box<dyn Future<Output = Result<TeamsAdmissionOutcome, TeamsDeliveryError>> + Send + 'a>,
        > {
            Box::pin(async {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(TeamsAdmissionOutcome::Committed)
            })
        }
    }

    #[tokio::test]
    async fn http_ingress_logs_rejections_without_changing_scoped_admission() {
        let fixture = crate::teams::edge::tests::ConnectorFixture::start().await;
        let deliveries = Arc::new(AtomicUsize::new(0));
        let bridge = Arc::new(TeamsResidentBridge::new(
            fixture.edge.clone(),
            TeamsDelivery::new(RecordedDelivery(deliveries.clone())),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let writer = CapturedLog(bytes.clone());
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let app = Router::new()
            .route("/api/messages", post(messages))
            .layer(from_fn(move |request: Request, next: Next| {
                let dispatch = dispatch.clone();
                async move { next.run(request).with_subscriber(dispatch).await }
            }))
            .with_state(bridge.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let valid_authorization = fixture.authorization();
        let cases = [
            (None, "missing_authorization_header"),
            (
                Some(axum::http::HeaderValue::from_bytes(&[0xff]).unwrap()),
                "invalid_authorization_header",
            ),
            (
                Some("Bearer private-token-sentinel".parse().unwrap()),
                "invalid_token_header",
            ),
            (
                Some(valid_authorization.parse().unwrap()),
                "invalid_activity_json",
            ),
        ];
        for (authorization, reason) in cases {
            let mut request = client
                .post(format!("http://{address}/api/messages"))
                .header("x-arr-log-id", "f1170039-ed04-48ab-b9a1-f4ee8ed07377")
                .body(r#"{"private-field-sentinel":"private-body-sentinel"}"#);
            if let Some(authorization) = authorization {
                request = request.header("authorization", authorization);
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{reason}");
            assert_eq!(response.text().await.unwrap(), "");
            let captured = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert!(captured.contains(reason), "{captured}");
        }
        let captured = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
        assert_eq!(
            captured.matches("Teams ingress rejected").count(),
            4,
            "{captured}"
        );
        assert!(
            captured.contains("f1170039-ed04-48ab-b9a1-f4ee8ed07377"),
            "{captured}"
        );
        for private in [
            "private-token-sentinel",
            "private-field-sentinel",
            "private-body-sentinel",
            &valid_authorization,
        ] {
            assert!(!captured.contains(private), "{captured}");
        }
        assert!(bridge.pending.lock().unwrap().is_empty());
        assert_eq!(deliveries.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.token_requests(), 0);
        assert!(fixture.replies().is_empty());

        let accepted = client
            .post(format!("http://{address}/api/messages"))
            .header("authorization", valid_authorization)
            .body(fixture.activity("accepted-activity"))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(bridge.pending.lock().unwrap().len(), 1);
        assert_eq!(fixture.token_requests(), 0);
        let handle = bridge
            .pending
            .lock()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let receipt = bridge.reply(&handle, "accepted reply").await.unwrap();
        assert_eq!(receipt.incoming_activity_id.as_str(), "accepted-activity");
        assert_eq!(
            receipt.outgoing_activity_id.as_str(),
            "outgoing-activity-id"
        );
        assert_eq!(fixture.replies().len(), 1);
        assert_eq!(fixture.replies()[0].1["text"], "accepted reply");
        assert!(matches!(
            bridge.reply(&handle, "duplicate reply").await,
            Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::UnknownReplyHandle
            })
        ));
        assert_eq!(fixture.replies().len(), 1);
        server.abort();
    }

    #[test]
    fn teams_error_display_fragments_start_lowercase() {
        assert_eq!(
            TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::ReplyRegistryFull
            }
            .to_string(),
            "teams reply registry is full"
        );
        assert_eq!(
            ReplyStoreError {
                kind: crate::teams::ReplyStoreErrorKind::Full
            }
            .to_string(),
            "teams reply registry is full"
        );
        assert_eq!(
            TeamsEdgeError {
                kind: crate::teams::TeamsEdgeErrorKind::MetadataTooLarge
            }
            .to_string(),
            "teams metadata response exceeds its size limit"
        );
        assert_eq!(
            crate::codex::LiveQueueError {
                kind: crate::codex::LiveQueueErrorKind::ReplayCapacity(
                    crate::codex::QueueError::new(
                        "resident admission",
                        crate::codex::QueueErrorKind::ReplayCapacity
                    )
                )
            }
            .to_string(),
            "teams replay-identity capacity is full"
        );
    }

    #[tokio::test]
    async fn owned_reply_task_preserves_panic_classification_without_public_payload() {
        let failed: tokio::task::JoinHandle<Result<LiveReplyReceipt, TeamsRuntimeError>> =
            tokio::spawn(async { panic!("private task panic sentinel") });
        let error = settle_reply(failed).await.unwrap_err();
        assert_eq!(error.to_string(), "teams reply task failed");
        assert!(
            matches!(error.kind, TeamsRuntimeErrorKind::ReplyTask(source) if source.is_panic())
        );
        let successful = tokio::spawn(async {
            Ok(LiveReplyReceipt {
                incoming_activity_id: "in".parse().unwrap(),
                outgoing_activity_id: "out".parse().unwrap(),
                conversation_id: "room".parse().unwrap(),
            })
        });
        assert_eq!(
            settle_reply(successful)
                .await
                .unwrap()
                .outgoing_activity_id
                .as_str(),
            "out"
        );
    }

    #[tokio::test]
    async fn lost_authority_task_result_is_not_a_preparation_rejection() {
        let task = tokio::task::spawn_blocking(|| panic!("lost authority response"));
        let cause = task.await.unwrap_err();
        let error = TeamsRuntimeError::from(ReplyStoreError {
            kind: ReplyStoreErrorKind::Task(cause),
        });
        assert_eq!(
            error.to_string(),
            "reply authority operation completion is unknown"
        );
        assert!(
            matches!(error.kind, TeamsRuntimeErrorKind::AuthorityCompletionUnknown(
            ReplyStoreError { kind: ReplyStoreErrorKind::Task(cause) }
        ) if cause.is_panic())
        );
        let rejected = TeamsRuntimeError::from(ReplyStoreError {
            kind: ReplyStoreErrorKind::UnknownHandle,
        });
        assert!(matches!(
            rejected.kind,
            TeamsRuntimeErrorKind::UnknownReplyHandle
        ));
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
            Err(TeamsEdgeError {
                kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint,
            })
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
            Err(TeamsEdgeError {
                kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint,
            })
        }
    }

    #[tokio::test]
    async fn cancellation_during_token_acquisition_keeps_reply_handle_retryable() {
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(
            &queue_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load queue");
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
                app_id: "app".parse().unwrap(),
                tenant_id: "tenant".parse().unwrap(),
                allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let bridge = Arc::new(TeamsResidentBridge::new(
            edge,
            crate::composition::teams_admission::from_codex(queue),
        ));
        bridge.pending.lock().expect("Teams reply registry").insert(
            ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
            PendingReply {
                envelope: AuthenticatedTeamsEnvelope {
                    reference: AuthenticatedConversationReference {
                        service_url: "https://connector.test/".parse().unwrap(),
                        channel_id: "msteams".to_owned(),
                        conversation_id: "conversation".parse().unwrap(),
                        incoming_activity_id: "activity".parse().unwrap(),
                        bot_id: "bot".parse().unwrap(),
                        sender_id: "sender".parse().unwrap(),
                        dione_authorized_tenant_id: "tenant".parse().unwrap(),
                    },
                    text: "hello".to_owned(),
                },
                expires_at: Instant::now() + REPLY_HANDLE_TTL,
            },
        );

        let in_flight = tokio::spawn({
            let bridge = bridge.clone();
            async move {
                bridge
                    .reply(
                        &ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
                        "reply",
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .expect("first token acquisition started");
        in_flight.abort();
        assert!(in_flight.await.expect_err("task aborted").is_cancelled());

        release.notify_one();
        assert!(matches!(
            bridge
                .reply(
                    &ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
                    "reply"
                )
                .await,
            Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint
                })
            })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn token_failure_before_send_keeps_reply_handle_retryable() {
        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = CodexEventQueue::load(
            &queue_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load queue");
        let attempts = Arc::new(AtomicUsize::new(0));
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            FailingTokenProvider(attempts.clone()),
            AdmissionPolicy {
                app_id: "app".parse().unwrap(),
                tenant_id: "tenant".parse().unwrap(),
                allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let bridge =
            TeamsResidentBridge::new(edge, crate::composition::teams_admission::from_codex(queue));
        bridge.pending.lock().expect("Teams reply registry").insert(
            ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
            PendingReply {
                envelope: AuthenticatedTeamsEnvelope {
                    reference: AuthenticatedConversationReference {
                        service_url: "https://connector.test/".parse().unwrap(),
                        channel_id: "msteams".to_owned(),
                        conversation_id: "conversation".parse().unwrap(),
                        incoming_activity_id: "activity".parse().unwrap(),
                        bot_id: "bot".parse().unwrap(),
                        sender_id: "sender".parse().unwrap(),
                        dione_authorized_tenant_id: "tenant".parse().unwrap(),
                    },
                    text: "hello".to_owned(),
                },
                expires_at: Instant::now() + REPLY_HANDLE_TTL,
            },
        );

        assert!(matches!(
            bridge
                .reply(
                    &ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
                    "reply"
                )
                .await,
            Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint
                })
            })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        let retry = bridge
            .reply(
                &ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000001").unwrap(),
                "reply",
            )
            .await
            .expect_err("token acquisition also fails on a safe retry");
        assert!(
            matches!(
                &retry,
                TeamsRuntimeError {
                    kind: TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                        kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint
                    })
                }
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
        let queue = CodexEventQueue::load(
            &state_dir,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load queue");
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut policy = AdmissionPolicy {
            app_id: "app".parse().unwrap(),
            tenant_id: "tenant".parse().unwrap(),
            allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
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
                conversation_id: "conversation".parse().unwrap(),
                incoming_activity_id: "activity".parse().unwrap(),
                bot_id: "bot".parse().unwrap(),
                sender_id: "sender".parse().unwrap(),
                dione_authorized_tenant_id: "tenant".parse().unwrap(),
            },
            text: "not stored".to_owned(),
        };
        let handle = ReplyHandle::generate();
        let now = chrono::Utc::now();
        let bridge = TeamsResidentBridge::new_durable(
            edge.clone(),
            crate::composition::teams_admission::from_codex(queue.clone()),
            &state_dir,
        )
        .await
        .expect("load durable bridge");
        let receipt = bridge
            .durable
            .as_ref()
            .expect("durable store")
            .insert(
                handle.clone(),
                SealedReplyAuthority::seal(
                    &envelope,
                    &"app".parse().unwrap(),
                    now + chrono::TimeDelta::minutes(15),
                ),
                now,
            )
            .await
            .expect("persist handle");
        assert!(matches!(receipt, StoreReceipt::Persisted(())));
        assert!(matches!(
            bridge.reply(&handle, "reply").await,
            Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint
                })
            })
        ));
        drop(bridge);
        let restored = TeamsResidentBridge::new_durable(
            edge,
            crate::composition::teams_admission::from_codex(queue),
            &state_dir,
        )
        .await
        .expect("restore handle after token failure");
        assert!(matches!(
            restored.reply(&handle, "reply").await,
            Err(TeamsRuntimeError {
                kind: TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: crate::teams::TeamsEdgeErrorKind::InvalidTokenEndpoint
                })
            })
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
        let queue = CodexEventQueue::load(
            &queue_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load queue");
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            UnusedTokenProvider,
            AdmissionPolicy {
                app_id: "app".parse().unwrap(),
                tenant_id: "tenant".parse().unwrap(),
                allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let lifecycle = CancellationToken::new();
        let error = run_listener(
            Arc::new(TeamsResidentBridge::new(
                edge,
                crate::composition::teams_admission::from_codex(queue),
            )),
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
        let queue = CodexEventQueue::load(
            &queue_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load queue");
        let edge = TeamsEdge::new(
            reqwest::Client::new(),
            UnusedTokenProvider,
            AdmissionPolicy {
                app_id: "app".parse().unwrap(),
                tenant_id: "tenant".parse().unwrap(),
                allowed_conversations: BTreeSet::from(["conversation".parse().unwrap()]),
                allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
                channels: Default::default(),
            },
        )
        .expect("construct edge");
        let lifecycle = CancellationToken::new();
        let task_lifecycle = lifecycle.clone();
        let mut listener = tokio::spawn(run_listener(
            Arc::new(TeamsResidentBridge::new(
                edge,
                crate::composition::teams_admission::from_codex(queue),
            )),
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
