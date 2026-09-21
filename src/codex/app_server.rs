use super::{CodexEventQueue, CodexQueueError, CodexThreadId, LeasedEvent};
use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{env, fmt::Debug, io, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream, client_async_with_config,
    tungstenite::{Error as WebSocketError, Message, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_WAIT: Duration = Duration::from_secs(45);
// Connection setup and the bounded idle or active request sequence can consume
// several request-timeout windows. Keep the lease well beyond that sequence so
// a successfully accepted turn can still be acknowledged.
const EVENT_LEASE: Duration = Duration::from_secs(5 * 60);
const MAX_WEBSOCKET_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// Result known at the app-server request/response boundary. Accepted means
/// Codex acknowledged the turn/start request, not that the recipient saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttentionDeliveryReceipt {
    Accepted,
    Uncertain,
}

/// Final dispatch denial: temporary evidence loss defers work; invalid evidence rejects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AttentionGuardFailure {
    #[error("attention evidence is temporarily unavailable")]
    Unavailable,
    #[error("attention evidence was invalidated")]
    Invalidated,
}

/// Final lifecycle fence and durable outcome observer for managed work.
pub trait AttentionDeliveryGuard: Send + Sync + Debug {
    /// Checks current authority immediately before dispatch; must not wait for blocking I/O.
    fn check(&self, notification: &Value) -> Result<(), AttentionGuardFailure>;
    /// Durably records a transport-boundary outcome before queue acknowledgement continues.
    ///
    /// The owned guard lets implementations move blocking persistence into an
    /// awaited worker without borrowing the delivery task.
    fn receipt<'a>(
        self: Arc<Self>,
        notification: &'a Value,
        outcome: AttentionDeliveryReceipt,
    ) -> BoxFuture<'a, Result<(), String>>;
}

#[derive(Debug, Clone)]
pub struct CodexDeliveryConfig {
    pub socket_path: Utf8PathBuf,
    pub request_timeout: Duration,
    pub preamble_mode: crate::config::PreambleMode,
    pub preamble_template: crate::config::PreambleTemplate,
    pub attention_guard: Option<Arc<dyn AttentionDeliveryGuard>>,
}

impl CodexDeliveryConfig {
    pub fn resolve(socket_path: Option<Utf8PathBuf>) -> Result<Self, CodexDeliveryError> {
        let socket_path = socket_path
            .or_else(|| {
                env::var("CODEX_APP_SERVER_SOCKET")
                    .ok()
                    .map(Utf8PathBuf::from)
            })
            .or_else(default_socket_path)
            .ok_or(CodexDeliveryError::MissingHome)?;
        let defaults = crate::config::DeliveryConfig::default();
        Ok(Self {
            socket_path,
            request_timeout: REQUEST_TIMEOUT,
            preamble_mode: defaults.preamble_mode,
            preamble_template: defaults.preamble_template,
            attention_guard: None,
        })
    }
}

fn default_socket_path() -> Option<Utf8PathBuf> {
    env::var("CODEX_HOME")
        .ok()
        .map(Utf8PathBuf::from)
        .or_else(|| {
            env::var("HOME")
                .ok()
                .map(|home| Utf8PathBuf::from(home).join(".codex"))
        })
        .map(|home| home.join("app-server-control/app-server-control.sock"))
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexDeliveryError {
    #[error("Codex live delivery requires an app-server socket path or HOME")]
    MissingHome,
    #[error("failed to connect to Codex app-server socket `{path}`")]
    Connect {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to establish Codex app-server WebSocket on `{path}`")]
    Handshake {
        path: Utf8PathBuf,
        #[source]
        source: Box<WebSocketError>,
    },
    #[error("Codex app-server WebSocket handshake on `{path}` timed out")]
    HandshakeTimeout { path: Utf8PathBuf },
    #[error("Codex app-server request `{method}` timed out")]
    Timeout { method: &'static str },
    #[error("Codex app-server closed the connection during `{method}`")]
    Disconnected { method: &'static str },
    #[error("Codex app-server rejected `{method}`: {message}")]
    Rejected {
        method: &'static str,
        message: String,
    },
    #[error("failed to exchange `{method}` with Codex app-server")]
    Protocol {
        method: &'static str,
        #[source]
        source: Box<WebSocketError>,
    },
    #[error("codex thread is active but its current turn id is unavailable")]
    ActiveTurnUnknown,
    #[error("codex thread status `{status}` does not support live delivery")]
    UnsupportedThreadStatus { status: String },
    #[error(transparent)]
    Queue(#[from] CodexQueueError),
}

#[derive(Debug, Error)]
enum AppServerRequestError {
    #[error(transparent)]
    Delivery(#[from] CodexDeliveryError),
    #[error("Codex app-server rejected `{method}`: {rendered}")]
    Rejected {
        method: &'static str,
        rendered: String,
        code: Option<i64>,
        rpc_message: Option<String>,
    },
}

impl AppServerRequestError {
    fn rejected(method: &'static str, error: &Value) -> Self {
        Self::Rejected {
            method,
            rendered: error.to_string(),
            code: error.get("code").and_then(Value::as_i64),
            rpc_message: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
        }
    }
}

impl From<AppServerRequestError> for CodexDeliveryError {
    fn from(error: AppServerRequestError) -> Self {
        match error {
            AppServerRequestError::Delivery(error) => error,
            AppServerRequestError::Rejected {
                method, rendered, ..
            } => CodexDeliveryError::Rejected {
                method,
                message: rendered,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeliveryOutcome {
    Dispatched,
    DeferredAttention,
    RejectedAttention { reason: String },
}

struct PendingAttentionReceipt {
    guard: Arc<dyn AttentionDeliveryGuard>,
    outcome: AttentionDeliveryReceipt,
}

struct AppServerClient {
    stream: WebSocketStream<UnixStream>,
    next_request_id: u64,
    config: CodexDeliveryConfig,
    thread_id: CodexThreadId,
}

impl AppServerClient {
    async fn connect(
        config: CodexDeliveryConfig,
        thread_id: CodexThreadId,
    ) -> Result<Self, CodexDeliveryError> {
        let unix_stream = UnixStream::connect(config.socket_path.as_std_path())
            .await
            .map_err(|source| CodexDeliveryError::Connect {
                path: config.socket_path.clone(),
                source,
            })?;
        let handshake =
            client_async_with_config("ws://localhost/", unix_stream, Some(websocket_config()));
        let (stream, _) = tokio::time::timeout(config.request_timeout, handshake)
            .await
            .map_err(|_| CodexDeliveryError::HandshakeTimeout {
                path: config.socket_path.clone(),
            })?
            .map_err(|source| CodexDeliveryError::Handshake {
                path: config.socket_path.clone(),
                source: Box::new(source),
            })?;
        let mut client = Self {
            stream,
            next_request_id: 0,
            config,
            thread_id,
        };
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "dione",
                        "title": "Dione",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "capabilities": { "experimentalApi": true }
                }),
            )
            .await?;
        client
            .send(
                "initialized",
                json!({ "method": "initialized", "params": {} }),
            )
            .await?;
        Ok(client)
    }

    /// Resolve the preamble for the current event without mutating state.
    ///
    /// `preamble_sent` indicates whether a preamble has already been
    /// successfully delivered on this binding. The caller is responsible
    /// for tracking and advancing that flag after a confirmed delivery.
    fn resolve_preamble(&self, preamble_sent: bool) -> Option<String> {
        use crate::config::PreambleMode;
        match self.config.preamble_mode {
            PreambleMode::Always => Some(self.config.preamble_template.as_str().to_owned()),
            PreambleMode::First if !preamble_sent => {
                Some(self.config.preamble_template.as_str().to_owned())
            }
            PreambleMode::First => None,
            PreambleMode::Never => None,
        }
    }

    #[cfg(test)]
    async fn deliver(
        &mut self,
        event: &LeasedEvent,
        preamble: Option<&str>,
    ) -> Result<DeliveryOutcome, CodexDeliveryError> {
        let mut receipt = None;
        let outcome = self.deliver_transport(event, preamble, &mut receipt).await;
        persist_attention_receipt(receipt, event).await;
        outcome
    }

    async fn deliver_transport(
        &mut self,
        event: &LeasedEvent,
        preamble: Option<&str>,
        pending_receipt: &mut Option<PendingAttentionReceipt>,
    ) -> Result<DeliveryOutcome, CodexDeliveryError> {
        let result = self
            .request(
                "thread/read",
                json!({
                    "threadId": self.thread_id,
                    "includeTurns": false
                }),
            )
            .await?;
        let thread = result.get("thread").cloned().unwrap_or(Value::Null);
        let status = thread.pointer("/status/type").and_then(Value::as_str);
        if status == Some("active") && event.attention.is_some() {
            // Attention-managed ambient work never steers an active turn. The
            // worker releases its lease so ordinary/direct work can pass it.
            return Ok(DeliveryOutcome::DeferredAttention);
        }

        let input = event_input(event, preamble);
        let client_message_id = event.attention.as_ref().map_or_else(
            || format!("dione-{}", event.event_id),
            |attention| format!("dione-attention-{}", attention.record_id),
        );
        match status {
            Some("idle") => {
                let guard = if event.attention.is_some() {
                    let Some(guard) = self.config.attention_guard.as_ref() else {
                        return Ok(DeliveryOutcome::DeferredAttention);
                    };
                    // This is deliberately after thread/read and immediately
                    // before the dispatch request: asynchronous work cannot
                    // turn a stale admission into an effect.
                    match guard.check(&event.event) {
                        Ok(()) => {}
                        Err(AttentionGuardFailure::Unavailable) => {
                            return Ok(DeliveryOutcome::DeferredAttention);
                        }
                        Err(error @ AttentionGuardFailure::Invalidated) => {
                            return Ok(DeliveryOutcome::RejectedAttention {
                                reason: error.to_string(),
                            });
                        }
                    }
                    Some(Arc::clone(guard))
                } else {
                    None
                };
                let dispatch = self
                    .request(
                        "turn/start",
                        json!({
                            "threadId": self.thread_id,
                            "clientUserMessageId": client_message_id,
                            "input": input
                        }),
                    )
                    .await;
                if let Some(guard) = guard {
                    let outcome = match &dispatch {
                        Ok(_) => Some(AttentionDeliveryReceipt::Accepted),
                        Err(AppServerRequestError::Delivery(_)) => {
                            Some(AttentionDeliveryReceipt::Uncertain)
                        }
                        // A JSON-RPC error response is a definitive negative
                        // acknowledgement, not an ambiguous transport loss.
                        Err(AppServerRequestError::Rejected { .. }) => None,
                    };
                    if let Some(outcome) = outcome {
                        *pending_receipt = Some(PendingAttentionReceipt { guard, outcome });
                    }
                }
                dispatch?;
            }
            Some("active") => {
                let turns = self
                    .request(
                        "thread/turns/list",
                        json!({
                            "threadId": self.thread_id,
                            "limit": 1,
                            "sortDirection": "desc",
                            "itemsView": "notLoaded"
                        }),
                    )
                    .await?;
                let turn_id = active_turn_id(&turns)?;
                let mut params = json!({
                    "threadId": self.thread_id,
                    "expectedTurnId": turn_id,
                    "clientUserMessageId": client_message_id,
                    "input": input
                });
                if let Err(error) = self.request("turn/steer", params.clone()).await {
                    let Some(current_turn_id) = stale_turn_mismatch(&error, &turn_id) else {
                        return Err(error.into());
                    };
                    params["expectedTurnId"] = Value::String(current_turn_id);
                    self.request("turn/steer", params).await?;
                }
            }
            status => {
                return Err(CodexDeliveryError::UnsupportedThreadStatus {
                    status: status.unwrap_or("missing").to_owned(),
                });
            }
        }
        Ok(DeliveryOutcome::Dispatched)
    }

    async fn request(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<Value, AppServerRequestError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        self.send(
            method,
            json!({ "method": method, "id": request_id, "params": params }),
        )
        .await?;

        tokio::time::timeout(self.config.request_timeout, async {
            loop {
                let Some(message) = self.stream.next().await else {
                    return Err(AppServerRequestError::from(
                        CodexDeliveryError::Disconnected { method },
                    ));
                };
                match message.map_err(|source| {
                    AppServerRequestError::from(CodexDeliveryError::Protocol {
                        method,
                        source: Box::new(source),
                    })
                })? {
                    Message::Text(text) => {
                        let message: Value = serde_json::from_str(&text).map_err(|source| {
                            AppServerRequestError::from(CodexDeliveryError::Rejected {
                                method,
                                message: format!("invalid JSON response: {source}"),
                            })
                        })?;
                        if message.get("id").and_then(Value::as_u64) != Some(request_id) {
                            continue;
                        }
                        if let Some(error) = message.get("error") {
                            let rejection = AppServerRequestError::rejected(method, error);
                            if let AppServerRequestError::Rejected {
                                method: "turn/steer",
                                code: Some(-32600),
                                rpc_message,
                                ..
                            } = &rejection
                                && rpc_message.as_deref() != Some("no active turn to steer")
                                && params
                                    .get("expectedTurnId")
                                    .and_then(Value::as_str)
                                    .and_then(|expected| stale_turn_mismatch(&rejection, expected))
                                    .is_none()
                            {
                                tracing::warn!(
                                    method,
                                    code = -32600,
                                    "unrecognized Codex steer rejection; check delivery error for changed server wording"
                                );
                            }
                            return Err(rejection);
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Message::Ping(payload) => self
                        .stream
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|source| {
                            AppServerRequestError::from(CodexDeliveryError::Protocol {
                                method,
                                source: Box::new(source),
                            })
                        })?,
                    Message::Close(_) => {
                        return Err(AppServerRequestError::from(
                            CodexDeliveryError::Disconnected { method },
                        ));
                    }
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        })
        .await
        .map_err(|_| AppServerRequestError::from(CodexDeliveryError::Timeout { method }))?
    }

    async fn send(
        &mut self,
        method: &'static str,
        message: Value,
    ) -> Result<(), CodexDeliveryError> {
        tokio::time::timeout(
            self.config.request_timeout,
            self.stream.send(Message::Text(message.to_string())),
        )
        .await
        .map_err(|_| CodexDeliveryError::Timeout { method })?
        .map_err(|source| CodexDeliveryError::Protocol {
            method,
            source: Box::new(source),
        })
    }
}

async fn persist_attention_receipt(receipt: Option<PendingAttentionReceipt>, event: &LeasedEvent) {
    let Some(PendingAttentionReceipt { guard, outcome }) = receipt else {
        return;
    };
    if let Err(error) = guard.receipt(&event.event, outcome).await {
        // The receipt sink is metadata, not the transport. In particular, a
        // confirmed accepted turn must not be retried solely because this
        // durable update failed.
        tracing::error!(
            event_id = %event.event_id,
            ?outcome,
            error,
            "failed to persist attention delivery receipt"
        );
    }
}

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        // Leave room for large bounded protocol responses above Tungstenite's
        // 16 MiB frame default without accepting unbounded messages.
        max_message_size: Some(MAX_WEBSOCKET_MESSAGE_SIZE),
        max_frame_size: Some(MAX_WEBSOCKET_MESSAGE_SIZE),
        max_write_buffer_size: MAX_WEBSOCKET_MESSAGE_SIZE + 128 * 1024,
        ..WebSocketConfig::default()
    }
}

fn active_turn_id(turns: &Value) -> Result<String, CodexDeliveryError> {
    turns
        .get("data")
        .and_then(Value::as_array)
        .and_then(|turns| (turns.len() == 1).then(|| &turns[0]))
        .filter(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or(CodexDeliveryError::ActiveTurnUnknown)
}

fn stale_turn_mismatch(error: &AppServerRequestError, expected_turn_id: &str) -> Option<String> {
    let AppServerRequestError::Rejected {
        method: "turn/steer",
        code,
        rpc_message,
        ..
    } = error
    else {
        return None;
    };
    if *code != Some(-32600) {
        return None;
    }
    let message = rpc_message.as_deref()?;
    let prefix = format!("expected active turn id `{expected_turn_id}` but found `");
    let current_turn_id = message.strip_prefix(&prefix)?.strip_suffix('`')?;
    (!current_turn_id.is_empty()
        && !current_turn_id.contains('`')
        && current_turn_id != expected_turn_id)
        .then(|| current_turn_id.to_owned())
}

fn event_input(event: &LeasedEvent, preamble: Option<&str>) -> Value {
    let prompt = match preamble {
        Some(pre) => format!("{pre}\n{}", event.event),
        None => event.event.to_string(),
    };
    json!([{ "type": "text", "text": prompt, "text_elements": [] }])
}

pub async fn run_delivery_worker(
    queue: CodexEventQueue,
    config: CodexDeliveryConfig,
    thread_binding: tokio::sync::watch::Receiver<Option<CodexThreadId>>,
    cancel: CancellationToken,
) -> Result<(), CodexDeliveryError> {
    run_delivery_worker_with_lease(queue, config, thread_binding, cancel, EVENT_LEASE).await
}

async fn run_delivery_worker_with_lease(
    queue: CodexEventQueue,
    config: CodexDeliveryConfig,
    mut thread_binding: tokio::sync::watch::Receiver<Option<CodexThreadId>>,
    cancel: CancellationToken,
    event_lease: Duration,
) -> Result<(), CodexDeliveryError> {
    let consumer_id = queue.register_live_consumer().await?;
    let mut client = None;
    let mut retry_delay = INITIAL_RETRY_DELAY;
    let mut attention_retry_delay = INITIAL_RETRY_DELAY;
    // Track first-mode preamble state outside the disposable WebSocket
    // client so it survives reconnects within the same thread binding.
    // Reset when the thread binding changes.
    let mut preamble_sent = false;

    loop {
        let Some(thread_id) = thread_binding.borrow_and_update().clone() else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                changed = thread_binding.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    continue;
                }
            }
        };
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            changed = thread_binding.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                client = None;
                preamble_sent = false;
                attention_retry_delay = INITIAL_RETRY_DELAY;
                continue;
            },
            event = queue.next_live_event(&consumer_id, &thread_id, EVENT_WAIT, event_lease) => event?,
        };
        let Some(mut event) = event else { continue };

        loop {
            if event.lease_expires_at <= chrono::Utc::now() {
                let replacement = queue
                    .next_live_event(&consumer_id, &thread_id, Duration::ZERO, event_lease)
                    .await?;
                let Some(replacement) = replacement else {
                    // The event may have been acknowledged or rerouted while
                    // its lease expired. Return to the durable queue instead
                    // of retrying a token that can no longer be acknowledged.
                    break;
                };
                tracing::debug!(
                    event_id = %replacement.event_id,
                    "renewed expired Codex live-delivery lease"
                );
                event = replacement;
            }

            if client.is_none() {
                let connection = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(()),
                    changed = thread_binding.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        preamble_sent = false;
                        attention_retry_delay = INITIAL_RETRY_DELAY;
                        break;
                    },
                    connection = AppServerClient::connect(config.clone(), thread_id.clone()) => connection,
                };
                match connection {
                    Ok(connected) => {
                        client = Some(connected);
                        // Recheck the lease after connection setup, which can
                        // consume several request timeouts during an outage.
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(event_id = %event.event_id, error = %error, "failed to connect Codex live delivery");
                        match wait_to_retry_or_rebind(&cancel, &mut thread_binding, retry_delay)
                            .await
                        {
                            RetryWait::Elapsed => {
                                retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                            }
                            RetryWait::BindingChanged => {
                                client = None;
                                preamble_sent = false;
                                retry_delay = INITIAL_RETRY_DELAY;
                                attention_retry_delay = INITIAL_RETRY_DELAY;
                                break;
                            }
                            RetryWait::Stopped => return Ok(()),
                        }
                        continue;
                    }
                }
            }

            let Some(active_client) = client.as_mut() else {
                continue;
            };
            let preamble = active_client.resolve_preamble(preamble_sent);
            let mut receipt = None;
            let delivery = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                changed = thread_binding.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    client = None;
                    preamble_sent = false;
                    attention_retry_delay = INITIAL_RETRY_DELAY;
                    break;
                },
                delivery = active_client.deliver_transport(
                    &event,
                    preamble.as_deref(),
                    &mut receipt,
                ) => delivery,
            };
            // Once transport has produced a receipt, complete its durable write
            // before observing cancellation, rebinding, retrying, or acknowledging.
            persist_attention_receipt(receipt, &event).await;
            match delivery {
                Ok(DeliveryOutcome::Dispatched) => {
                    // A successful request is only an app-server acceptance,
                    // not proof that the recipient observed the notification.
                    if preamble.is_some() {
                        preamble_sent = true;
                    }
                    acknowledge_with_retry(&queue, &consumer_id, &event, &cancel).await?;
                    retry_delay = INITIAL_RETRY_DELAY;
                    if event.attention.is_some() {
                        attention_retry_delay = INITIAL_RETRY_DELAY;
                    }
                    break;
                }
                Ok(DeliveryOutcome::DeferredAttention) => {
                    let Some(attention) = event.attention.as_ref() else {
                        return Err(CodexQueueError::NotAttentionManaged.into());
                    };
                    match queue
                        .defer_attention(&consumer_id, &event.delivery_token, attention_retry_delay)
                        .await
                    {
                        Ok(_) => {}
                        Err(CodexQueueError::UnknownDeliveryToken) => {
                            tracing::debug!(
                                event_id = %event.event_id,
                                "attention-managed Codex event was invalidated while being deferred"
                            );
                            retry_delay = INITIAL_RETRY_DELAY;
                            attention_retry_delay = INITIAL_RETRY_DELAY;
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                    tracing::debug!(
                        event_id = %event.event_id,
                        scheduling = ?attention.delivery,
                        retry_after_ms = attention_retry_delay.as_millis(),
                        "deferred attention-managed Codex event while its thread is active"
                    );
                    attention_retry_delay = (attention_retry_delay * 2).min(MAX_RETRY_DELAY);
                    retry_delay = INITIAL_RETRY_DELAY;
                    break;
                }
                Ok(DeliveryOutcome::RejectedAttention { reason }) => {
                    let Some(attention) = event.attention.as_ref() else {
                        return Err(CodexQueueError::NotAttentionManaged.into());
                    };
                    let invalidated = queue
                        .invalidate_attention(None, Some(&attention.record_id))
                        .await?;
                    tracing::warn!(
                        event_id = %event.event_id,
                        record_id = %attention.record_id,
                        removed = invalidated.removed,
                        invalidated_leases = invalidated.invalidated_leases,
                        reason,
                        "rejected stale attention-managed Codex event before dispatch"
                    );
                    retry_delay = INITIAL_RETRY_DELAY;
                    attention_retry_delay = INITIAL_RETRY_DELAY;
                    break;
                }
                Err(error) => {
                    tracing::warn!(event_id = %event.event_id, error = %error, "failed to deliver live Codex event; retrying before later events");
                    client = None;
                    match wait_to_retry_or_rebind(&cancel, &mut thread_binding, retry_delay).await {
                        RetryWait::Elapsed => {
                            retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                        }
                        RetryWait::BindingChanged => {
                            preamble_sent = false;
                            retry_delay = INITIAL_RETRY_DELAY;
                            attention_retry_delay = INITIAL_RETRY_DELAY;
                            break;
                        }
                        RetryWait::Stopped => return Ok(()),
                    }
                }
            }
        }
    }
}

async fn acknowledge_with_retry(
    queue: &CodexEventQueue,
    consumer_id: &super::ConsumerId,
    event: &LeasedEvent,
    cancel: &CancellationToken,
) -> Result<(), CodexDeliveryError> {
    let mut delay = INITIAL_RETRY_DELAY;
    loop {
        match queue.acknowledge(consumer_id, &event.delivery_token).await {
            Ok(()) => return Ok(()),
            Err(CodexQueueError::UnknownDeliveryToken) => {
                // The durable event may already have been acknowledged, or a
                // newer lease may own it. In either case this token can never
                // succeed. Return to the queue; a still-pending event will be
                // redelivered with the same clientUserMessageId.
                tracing::warn!(
                    event_id = %event.event_id,
                    "Codex live-delivery acknowledgement token is no longer current"
                );
                return Ok(());
            }
            Err(error) if !matches!(error, CodexQueueError::InboxIo { .. }) => {
                return Err(error.into());
            }
            Err(error) => {
                tracing::error!(event_id = %event.event_id, error = %error, "failed to persist live Codex acknowledgement; retrying");
                wait_to_retry(cancel, delay).await;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                delay = (delay * 2).min(MAX_RETRY_DELAY);
            }
        }
    }
}

async fn wait_to_retry(cancel: &CancellationToken, delay: Duration) {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {},
        _ = tokio::time::sleep(delay) => {},
    }
}

enum RetryWait {
    Elapsed,
    BindingChanged,
    Stopped,
}

async fn wait_to_retry_or_rebind(
    cancel: &CancellationToken,
    thread_binding: &mut tokio::sync::watch::Receiver<Option<CodexThreadId>>,
    delay: Duration,
) -> RetryWait {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => RetryWait::Stopped,
        changed = thread_binding.changed() => {
            if changed.is_ok() {
                RetryWait::BindingChanged
            } else {
                RetryWait::Stopped
            }
        },
        _ = tokio::time::sleep(delay) => RetryWait::Elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{AttentionDelivery, AttentionScheduling, ConsumerId, DeliveryToken};
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tempfile::TempDir;
    use tokio::{net::UnixListener, sync::mpsc};
    use tokio_tungstenite::accept_async;

    fn test_delivery_config(socket_path: Utf8PathBuf, timeout: Duration) -> CodexDeliveryConfig {
        let defaults = crate::config::DeliveryConfig::default();
        CodexDeliveryConfig {
            socket_path,
            request_timeout: timeout,
            preamble_mode: defaults.preamble_mode,
            preamble_template: defaults.preamble_template,
            attention_guard: None,
        }
    }

    #[derive(Debug)]
    struct TestGuard {
        calls: Arc<AtomicUsize>,
        rejection: Option<AttentionGuardFailure>,
        receipts: Arc<StdMutex<Vec<AttentionDeliveryReceipt>>>,
        receipt_rejection: Option<&'static str>,
    }

    impl AttentionDeliveryGuard for TestGuard {
        fn check(&self, _notification: &Value) -> Result<(), AttentionGuardFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.rejection {
                Some(reason) => Err(reason),
                None => Ok(()),
            }
        }

        fn receipt<'a>(
            self: Arc<Self>,
            _notification: &'a Value,
            outcome: AttentionDeliveryReceipt,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.receipts.lock().unwrap().push(outcome);
                match self.receipt_rejection {
                    Some(reason) => Err(reason.to_owned()),
                    None => Ok(()),
                }
            })
        }
    }

    fn managed_test_event(delivery: AttentionDelivery, record_id: &str) -> LeasedEvent {
        let mut event = test_event();
        let delivery_marker = match delivery {
            AttentionDelivery::Prompt => "prompt",
            AttentionDelivery::NextTurn => "next_turn",
        };
        event.event["params"]["meta"] = json!({
            "attention_delivery": delivery_marker,
            "attention_record": record_id,
        });
        event.attention = Some(AttentionScheduling {
            delivery,
            record_id: record_id.into(),
        });
        event
    }

    fn managed_notification(message_id: &str, record_id: &str) -> Value {
        json!({
            "method": "notifications/claude/channel",
            "params": {
                "content": format!("managed {record_id}"),
                "meta": {
                    "message_id": message_id,
                    "attention_delivery": "next_turn",
                    "attention_record": record_id,
                }
            }
        })
    }

    fn rejected_error(
        method: &'static str,
        code: i64,
        message: impl Into<String>,
    ) -> AppServerRequestError {
        AppServerRequestError::rejected(method, &json!({ "code": code, "message": message.into() }))
    }

    #[test]
    fn finds_in_progress_turn() {
        let turns = json!({
            "data": [{ "id": "live", "status": "inProgress" }]
        });
        assert_eq!(active_turn_id(&turns).unwrap(), "live");
    }

    #[test]
    fn refuses_active_thread_without_turn_id() {
        let turns = json!({ "data": [{ "id": "done", "status": "completed" }] });
        assert!(matches!(
            active_turn_id(&turns),
            Err(CodexDeliveryError::ActiveTurnUnknown)
        ));
    }

    #[test]
    fn refuses_incoherent_multi_turn_bounded_response() {
        let turns = json!({
            "data": [
                { "id": "newest", "status": "inProgress" },
                { "id": "unexpected", "status": "completed" }
            ]
        });
        assert!(matches!(
            active_turn_id(&turns),
            Err(CodexDeliveryError::ActiveTurnUnknown)
        ));
    }

    #[test]
    fn stale_turn_mismatch_discriminator_fails_closed() {
        let canonical = rejected_error(
            "turn/steer",
            -32600,
            "expected active turn id `turn-stale` but found `turn-current`",
        );
        assert_eq!(
            stale_turn_mismatch(&canonical, "turn-stale").as_deref(),
            Some("turn-current")
        );

        let near_misses = [
            rejected_error(
                "turn/steer",
                -32602,
                "expected active turn id `turn-stale` but found `turn-current`",
            ),
            rejected_error(
                "turn/start",
                -32600,
                "expected active turn id `turn-stale` but found `turn-current`",
            ),
            rejected_error("turn/steer", -32600, "no active turn to steer"),
            rejected_error(
                "turn/steer",
                -32600,
                "expected active turn id `turn-stale` but found `turn-current",
            ),
            rejected_error(
                "turn/steer",
                -32600,
                "expected active turn id `turn-other` but found `turn-current`",
            ),
            rejected_error(
                "turn/steer",
                -32600,
                "expected active turn id `turn-stale` but found ``",
            ),
            rejected_error(
                "turn/steer",
                -32600,
                "expected active turn id `turn-stale` but found `turn-stale`",
            ),
            rejected_error("turn/steer", -32600, "unrelated rejection"),
            AppServerRequestError::from(CodexDeliveryError::Rejected {
                method: "turn/steer",
                message: "not JSON".to_owned(),
            }),
        ];
        for error in near_misses {
            assert_eq!(stale_turn_mismatch(&error, "turn-stale"), None);
        }
    }

    #[test]
    fn websocket_limits_are_bounded_above_tungstenite_frame_default() {
        let config = websocket_config();
        assert_eq!(config.max_message_size, Some(MAX_WEBSOCKET_MESSAGE_SIZE));
        assert_eq!(config.max_frame_size, Some(MAX_WEBSOCKET_MESSAGE_SIZE));
        assert_eq!(
            config.max_write_buffer_size,
            MAX_WEBSOCKET_MESSAGE_SIZE + 128 * 1024
        );
        const { assert!(MAX_WEBSOCKET_MESSAGE_SIZE > 16 * 1024 * 1024) };
    }

    #[tokio::test]
    async fn times_out_stalled_websocket_handshake() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let config = test_delivery_config(socket_path.clone(), Duration::from_millis(100));

        let error = match AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-stalled-handshake").unwrap(),
        )
        .await
        {
            Ok(_) => panic!("stalled handshake unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            CodexDeliveryError::HandshakeTimeout { path } if path == socket_path
        ));
        server.abort();
    }

    #[tokio::test]
    async fn times_out_when_upgraded_peer_stops_reading() {
        const OUTGOING_SIZE: usize = 8 * 1024 * 1024;

        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                if request["method"] == "initialized" {
                    std::future::pending::<()>().await;
                }
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                websocket
                    .send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                    .await
                    .unwrap();
            }
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(5));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-stalled-write").unwrap(),
        )
        .await
        .unwrap();
        client.config.request_timeout = Duration::from_millis(100);

        let error = client
            .send(
                "stalled/write",
                json!({ "payload": "x".repeat(OUTGOING_SIZE) }),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            CodexDeliveryError::Timeout {
                method: "stalled/write"
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn accepts_single_frame_larger_than_tungstenite_default() {
        const PAYLOAD_SIZE: usize = 16 * 1024 * 1024 + 1;

        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                if request["method"] == "initialized" {
                    break;
                }
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if request["method"] == "initialize" {
                    json!({ "payload": "x".repeat(PAYLOAD_SIZE) })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(5));

        AppServerClient::connect(config, CodexThreadId::parse("thread-large").unwrap())
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn delivers_over_unix_websocket_and_starts_idle_turn() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut received = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                received.push(request.clone());
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" }, "turns": [] } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
                if request["method"] == "turn/start" {
                    return received;
                }
            }
            received
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(1));
        let event = LeasedEvent {
            event_id: crate::codex::EventId::new(7),
            delivery_token: DeliveryToken::parse("token-7").unwrap(),
            lease_expires_at: chrono::Utc::now(),
            consumer_id: ConsumerId::parse("consumer-7").unwrap(),
            event: json!({ "params": { "content": "ping" } }),
            attention: None,
        };

        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-123").unwrap())
                .await
                .unwrap();
        client.deliver(&event, None).await.unwrap();

        let received = server.await.unwrap();
        let methods: Vec<_> = received
            .iter()
            .filter_map(|request| request.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            ["initialize", "initialized", "thread/read", "turn/start"]
        );
        assert_eq!(
            received[0]["params"]["capabilities"]["experimentalApi"],
            true
        );
        assert_eq!(received[2]["params"]["includeTurns"], false);
        assert_eq!(received[3]["params"]["threadId"], "thread-123");
        assert_eq!(received[3]["params"]["clientUserMessageId"], "dione-7");
    }

    #[tokio::test]
    async fn active_delivery_lists_only_newest_turn_without_items_and_steers_it() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut received = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                received.push(request.clone());
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = match request["method"].as_str().unwrap() {
                    "thread/read" => {
                        json!({ "thread": { "status": { "type": "active" } } })
                    }
                    "thread/turns/list" => json!({
                        "data": [{ "id": "turn-live", "status": "inProgress" }],
                        "nextCursor": "older",
                        "backwardsCursor": null
                    }),
                    _ => json!({}),
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
                if request["method"] == "turn/steer" {
                    return received;
                }
            }
            received
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(1));
        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-active").unwrap())
                .await
                .unwrap();

        client.deliver(&test_event(), None).await.unwrap();

        let received = server.await.unwrap();
        let methods: Vec<_> = received
            .iter()
            .filter_map(|request| request.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "thread/read",
                "thread/turns/list",
                "turn/steer"
            ]
        );
        assert_eq!(
            received[3]["params"],
            json!({
                "threadId": "thread-active",
                "limit": 1,
                "sortDirection": "desc",
                "itemsView": "notLoaded"
            })
        );
        assert_eq!(received[4]["params"]["expectedTurnId"], "turn-live");
    }

    #[tokio::test]
    async fn active_attention_delivery_defers_without_turn_mutation() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            while let Some(message) = websocket.next().await {
                let text = match message.unwrap() {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "active" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
            methods
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(1));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-managed-active").unwrap(),
        )
        .await
        .unwrap();

        let outcome = client
            .deliver(
                &managed_test_event(AttentionDelivery::Prompt, "record-active"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, DeliveryOutcome::DeferredAttention);
        client.stream.close(None).await.unwrap();
        drop(client);
        assert_eq!(
            server.await.unwrap(),
            ["initialize", "initialized", "thread/read"]
        );
    }

    #[tokio::test]
    async fn idle_attention_delivery_checks_guard_then_starts_turn() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut received = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                received.push(request.clone());
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
                if request["method"] == "turn/start" {
                    return received;
                }
            }
            received
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(StdMutex::new(Vec::new()));
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: calls.clone(),
            rejection: None,
            receipts: receipts.clone(),
            receipt_rejection: Some("metadata store unavailable"),
        }));
        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-managed-idle").unwrap())
                .await
                .unwrap();

        let outcome = client
            .deliver(
                &managed_test_event(AttentionDelivery::NextTurn, "record-idle"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(outcome, DeliveryOutcome::Dispatched);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *receipts.lock().unwrap(),
            [AttentionDeliveryReceipt::Accepted]
        );
        let received = server.await.unwrap();
        assert_eq!(received[3]["method"], "turn/start");
    }

    #[tokio::test]
    async fn managed_turn_identity_survives_reconstructed_queue_event_ids() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut client_message_ids = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let method = request["method"].as_str().unwrap();
                let result = if method == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                if method == "turn/start" {
                    client_message_ids.push(
                        request["params"]["clientUserMessageId"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    );
                }
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
                if client_message_ids.len() == 3 {
                    return client_message_ids;
                }
            }
            client_message_ids
        });
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: Arc::new(AtomicUsize::new(0)),
            rejection: None,
            receipts: Arc::new(StdMutex::new(Vec::new())),
            receipt_rejection: None,
        }));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-managed-identity").unwrap(),
        )
        .await
        .unwrap();
        let mut first = managed_test_event(AttentionDelivery::NextTurn, "stable-record");
        first.event_id = crate::codex::EventId::new(4);
        let mut reconstructed = managed_test_event(AttentionDelivery::NextTurn, "stable-record");
        reconstructed.event_id = crate::codex::EventId::new(40);
        let mut distinct = managed_test_event(AttentionDelivery::NextTurn, "distinct-record");
        distinct.event_id = crate::codex::EventId::new(41);

        for event in [&first, &reconstructed, &distinct] {
            assert_eq!(
                client.deliver(event, None).await.unwrap(),
                DeliveryOutcome::Dispatched
            );
        }

        let client_message_ids = server.await.unwrap();
        assert_eq!(client_message_ids.len(), 3);
        assert_eq!(client_message_ids[0], client_message_ids[1]);
        assert_ne!(client_message_ids[1], client_message_ids[2]);
    }

    #[tokio::test]
    async fn lost_turn_start_response_reports_uncertain_receipt() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if request["method"] == "turn/start" {
                    // The request reached Codex, but its response is lost.
                    return;
                }
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(StdMutex::new(Vec::new()));
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: calls.clone(),
            rejection: None,
            receipts: receipts.clone(),
            receipt_rejection: None,
        }));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-managed-uncertain").unwrap(),
        )
        .await
        .unwrap();

        assert!(
            client
                .deliver(
                    &managed_test_event(AttentionDelivery::Prompt, "record-uncertain"),
                    None,
                )
                .await
                .is_err()
        );
        server.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *receipts.lock().unwrap(),
            [AttentionDeliveryReceipt::Uncertain]
        );
    }

    #[tokio::test]
    async fn explicit_turn_start_rejection_is_not_an_uncertain_receipt() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                if request["method"] == "turn/start" {
                    websocket
                        .send(Message::Text(
                            json!({
                                "id": id,
                                "error": { "code": -32000, "message": "turn refused" }
                            })
                            .to_string(),
                        ))
                        .await
                        .unwrap();
                    return;
                }
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(StdMutex::new(Vec::new()));
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: calls.clone(),
            rejection: None,
            receipts: receipts.clone(),
            receipt_rejection: None,
        }));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-managed-rejected").unwrap(),
        )
        .await
        .unwrap();

        let error = client
            .deliver(
                &managed_test_event(AttentionDelivery::Prompt, "record-rejected"),
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CodexDeliveryError::Rejected { .. }));
        server.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(receipts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stale_attention_guard_rejects_before_idle_dispatch() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut methods = Vec::new();
            while let Some(message) = websocket.next().await {
                let text = match message.unwrap() {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                methods.push(request["method"].as_str().unwrap().to_owned());
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if request["method"] == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
            methods
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(StdMutex::new(Vec::new()));
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: calls.clone(),
            rejection: Some(AttentionGuardFailure::Invalidated),
            receipts: receipts.clone(),
            receipt_rejection: None,
        }));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-managed-stale").unwrap(),
        )
        .await
        .unwrap();

        let outcome = client
            .deliver(
                &managed_test_event(AttentionDelivery::Prompt, "record-stale"),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(outcome, DeliveryOutcome::RejectedAttention { .. }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(receipts.lock().unwrap().is_empty());
        client.stream.close(None).await.unwrap();
        drop(client);
        assert_eq!(
            server.await.unwrap(),
            ["initialize", "initialized", "thread/read"]
        );
    }

    #[tokio::test]
    async fn stale_active_turn_retries_once_with_server_reported_turn() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut steer_requests = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let response = match request["method"].as_str().unwrap() {
                    "thread/read" => {
                        json!({ "id": id, "result": { "thread": { "status": { "type": "active" } } } })
                    }
                    "thread/turns/list" => json!({
                        "id": id,
                        "result": {
                            "data": [{ "id": "turn-stale", "status": "inProgress" }]
                        }
                    }),
                    "turn/steer" => {
                        steer_requests.push(request.clone());
                        if steer_requests.len() == 1 {
                            json!({
                                "id": id,
                                "error": {
                                    "code": -32600,
                                    "message": "expected active turn id `turn-stale` but found `turn-current`"
                                }
                            })
                        } else {
                            json!({ "id": id, "result": {} })
                        }
                    }
                    _ => json!({ "id": id, "result": {} }),
                };
                websocket
                    .send(Message::Text(response.to_string()))
                    .await
                    .unwrap();
                if steer_requests.len() == 2 {
                    return steer_requests;
                }
            }
            steer_requests
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(1));
        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-stale-turn").unwrap())
                .await
                .unwrap();

        client.deliver(&test_event(), None).await.unwrap();

        let steer_requests = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("timed out waiting for stale-turn retry")
            .unwrap();
        assert_eq!(steer_requests.len(), 2);
        assert_eq!(steer_requests[0]["params"]["expectedTurnId"], "turn-stale");
        assert_eq!(
            steer_requests[1]["params"]["expectedTurnId"],
            "turn-current"
        );
        let expected_input = event_input(&test_event(), None);
        for request in &steer_requests {
            assert_eq!(request["params"]["clientUserMessageId"], "dione-99");
            assert_eq!(request["params"]["input"], expected_input);
        }
    }

    #[tokio::test]
    async fn second_stale_turn_mismatch_fails_closed_after_one_retry() {
        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut steer_requests = Vec::new();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let response = match request["method"].as_str().unwrap() {
                    "thread/read" => {
                        json!({ "id": id, "result": { "thread": { "status": { "type": "active" } } } })
                    }
                    "thread/turns/list" => json!({
                        "id": id,
                        "result": {
                            "data": [{ "id": "turn-stale", "status": "inProgress" }]
                        }
                    }),
                    "turn/steer" => {
                        steer_requests.push(request.clone());
                        let (expected, current) = if steer_requests.len() == 1 {
                            ("turn-stale", "turn-current")
                        } else {
                            ("turn-current", "turn-newer")
                        };
                        json!({
                            "id": id,
                            "error": {
                                "code": -32600,
                                "message": format!(
                                    "expected active turn id `{expected}` but found `{current}`"
                                )
                            }
                        })
                    }
                    _ => json!({ "id": id, "result": {} }),
                };
                websocket
                    .send(Message::Text(response.to_string()))
                    .await
                    .unwrap();
                if steer_requests.len() == 2 {
                    return steer_requests;
                }
            }
            steer_requests
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(1));
        let mut client = AppServerClient::connect(
            config,
            CodexThreadId::parse("thread-second-stale-turn").unwrap(),
        )
        .await
        .unwrap();

        let error = client.deliver(&test_event(), None).await.unwrap_err();

        assert!(matches!(
            error,
            CodexDeliveryError::Rejected {
                method: "turn/steer",
                ..
            }
        ));
        let steer_requests = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("timed out waiting for bounded stale-turn retry")
            .unwrap();
        assert_eq!(steer_requests.len(), 2);
        assert_eq!(steer_requests[0]["params"]["expectedTurnId"], "turn-stale");
        assert_eq!(
            steer_requests[1]["params"]["expectedTurnId"],
            "turn-current"
        );
    }

    #[tokio::test]
    async fn incoherent_bounded_state_fails_closed_before_turn_mutation() {
        enum ExpectedError {
            ActiveTurnUnknown,
            UnsupportedThreadStatus(&'static str),
        }

        let cases = [
            (
                "missing status",
                json!({ "thread": {} }),
                None,
                ExpectedError::UnsupportedThreadStatus("missing"),
            ),
            (
                "unsupported status",
                json!({ "thread": { "status": { "type": "notLoaded" } } }),
                None,
                ExpectedError::UnsupportedThreadStatus("notLoaded"),
            ),
            (
                "active empty turn list",
                json!({ "thread": { "status": { "type": "active" } } }),
                Some(json!({ "data": [] })),
                ExpectedError::ActiveTurnUnknown,
            ),
            (
                "active malformed turn list",
                json!({ "thread": { "status": { "type": "active" } } }),
                Some(json!({ "data": [{ "status": "inProgress" }] })),
                ExpectedError::ActiveTurnUnknown,
            ),
        ];

        for (name, thread_result, turns_result, expected_error) in cases {
            let dir = TempDir::new().unwrap();
            let socket_path =
                Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
            let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
            let expects_turn_list = turns_result.is_some();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut websocket = accept_async(stream).await.unwrap();
                let mut received = Vec::new();
                while let Some(message) = websocket.next().await {
                    let Message::Text(text) = message.unwrap() else {
                        continue;
                    };
                    let request: Value = serde_json::from_str(&text).unwrap();
                    let method = request["method"].as_str().unwrap().to_owned();
                    received.push(method.clone());
                    let Some(id) = request.get("id").and_then(Value::as_u64) else {
                        continue;
                    };
                    let result = match method.as_str() {
                        "thread/read" => thread_result.clone(),
                        "thread/turns/list" => turns_result.clone().unwrap(),
                        _ => json!({}),
                    };
                    websocket
                        .send(Message::Text(
                            json!({ "id": id, "result": result }).to_string(),
                        ))
                        .await
                        .unwrap();
                    let terminal_method = if expects_turn_list {
                        "thread/turns/list"
                    } else {
                        "thread/read"
                    };
                    if method == terminal_method {
                        return received;
                    }
                }
                received
            });
            let config = test_delivery_config(socket_path, Duration::from_secs(1));
            let mut client =
                AppServerClient::connect(config, CodexThreadId::parse("thread-invalid").unwrap())
                    .await
                    .unwrap();

            let error = client.deliver(&test_event(), None).await.unwrap_err();

            match expected_error {
                ExpectedError::ActiveTurnUnknown => {
                    assert!(
                        matches!(error, CodexDeliveryError::ActiveTurnUnknown),
                        "{name}"
                    );
                }
                ExpectedError::UnsupportedThreadStatus(expected) => assert!(
                    matches!(
                        error,
                        CodexDeliveryError::UnsupportedThreadStatus { status }
                            if status == expected
                    ),
                    "{name}"
                ),
            }
            let received = server.await.unwrap();
            let expected_methods = if expects_turn_list {
                vec![
                    "initialize",
                    "initialized",
                    "thread/read",
                    "thread/turns/list",
                ]
            } else {
                vec!["initialize", "initialized", "thread/read"]
            };
            assert_eq!(received, expected_methods, "{name}");
            assert!(
                !received
                    .iter()
                    .any(|method| method == "turn/start" || method == "turn/steer"),
                "{name}: incoherent state must not mutate a turn"
            );
        }
    }

    /// Falsifies the hypothesis that delivery must load full history before it
    /// can append to an idle thread. The fake server has a response larger than
    /// the client's 64 MiB message bound ready behind either legacy request;
    /// the old resume/includeTurns path reaches it and makes this test fail.
    #[tokio::test]
    async fn delivery_does_not_reach_available_history_larger_than_64_mib() {
        const HISTORY_SIZE: usize = MAX_WEBSOCKET_MESSAGE_SIZE + 1024;

        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let historical_payload = "h".repeat(HISTORY_SIZE);
            assert!(historical_payload.len() > MAX_WEBSOCKET_MESSAGE_SIZE);
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            let mut forbidden_history_requested = false;
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let include_full_history = method == "thread/resume"
                    || (method == "thread/read"
                        && request["params"]["includeTurns"].as_bool() == Some(true));
                let result = if include_full_history {
                    forbidden_history_requested = true;
                    json!({
                        "thread": {
                            "status": { "type": "idle" },
                            "turns": [{ "items": [{ "text": historical_payload.as_str() }] }]
                        }
                    })
                } else if method == "thread/read" {
                    json!({ "thread": { "status": { "type": "idle" } } })
                } else {
                    json!({})
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
                if method == "turn/start" {
                    return forbidden_history_requested;
                }
            }
            forbidden_history_requested
        });
        let config = test_delivery_config(socket_path, Duration::from_secs(5));
        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-huge").unwrap())
                .await
                .unwrap();

        client.deliver(&test_event(), None).await.unwrap();

        assert!(!server.await.unwrap());
    }

    #[tokio::test]
    async fn deferred_attention_allows_direct_event_to_bypass_active_turn() {
        let dir = TempDir::new().unwrap();
        let state_path = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let (steered_tx, mut steered_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Ok(Message::Text(text)) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let method = request["method"].as_str().unwrap();
                let result = match method {
                    "thread/read" => {
                        json!({ "thread": { "status": { "type": "active" } } })
                    }
                    "thread/turns/list" => json!({
                        "data": [{ "id": "turn-live", "status": "inProgress" }]
                    }),
                    "turn/steer" => {
                        steered_tx
                            .send(
                                request["params"]["clientUserMessageId"]
                                    .as_str()
                                    .unwrap()
                                    .to_owned(),
                            )
                            .unwrap();
                        json!({})
                    }
                    _ => json!({}),
                };
                websocket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string(),
                    ))
                    .await
                    .unwrap();
            }
        });

        let queue = CodexEventQueue::load(&state_path).unwrap();
        let thread_id = CodexThreadId::parse("thread-direct-bypass").unwrap();
        queue
            .bind_live_thread(Some(thread_id.clone()))
            .await
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let receipts = Arc::new(StdMutex::new(Vec::new()));
        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.attention_guard = Some(Arc::new(TestGuard {
            calls: calls.clone(),
            rejection: None,
            receipts: receipts.clone(),
            receipt_rejection: None,
        }));
        let cancel = CancellationToken::new();
        let (_binding_tx, binding_rx) = tokio::sync::watch::channel(Some(thread_id));
        let worker = tokio::spawn(run_delivery_worker_with_lease(
            queue.clone(),
            config,
            binding_rx,
            cancel.clone(),
            Duration::from_secs(30),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue.status().await.primary_consumer.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        queue
            .enqueue(managed_notification("201", "record-held"))
            .await
            .unwrap();
        queue
            .enqueue(json!({
                "method": "notifications/claude/channel",
                "params": {
                    "content": "direct event",
                    "meta": { "message_id": "202" }
                }
            }))
            .await
            .unwrap();

        let steered = tokio::time::timeout(Duration::from_secs(2), steered_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(steered, "dione-1");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let status = queue.status().await;
                if status.queued == 1 && status.leased == 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(receipts.lock().unwrap().is_empty());

        cancel.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn reset_peer_reacquires_expired_lease_and_retries_before_later_events() {
        let dir = TempDir::new().unwrap();
        let state_path = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let reset_first = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn({
            let reset_first = reset_first.clone();
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut websocket = accept_async(stream).await.unwrap();
                    while let Some(message) = websocket.next().await {
                        let Ok(Message::Text(text)) = message else {
                            break;
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let Some(id) = request.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let method = request["method"].as_str().unwrap();
                        if method == "turn/start" {
                            let message_id = request["params"]["clientUserMessageId"]
                                .as_str()
                                .unwrap()
                                .to_owned();
                            started_tx.send(message_id).unwrap();
                            if !reset_first.swap(true, Ordering::SeqCst) {
                                // Simulate the app-server peer vanishing after
                                // it receives a request but before Dione sees
                                // the response.
                                break;
                            }
                        }
                        let response = if method == "thread/read" {
                            json!({ "id": id, "result": { "thread": { "status": { "type": "idle" }, "turns": [] } } })
                        } else {
                            json!({ "id": id, "result": {} })
                        };
                        websocket
                            .send(Message::Text(response.to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let queue = CodexEventQueue::load(&state_path).unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-order").unwrap()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let (_binding_tx, binding_rx) =
            tokio::sync::watch::channel(Some(CodexThreadId::parse("thread-order").unwrap()));
        let worker = tokio::spawn(run_delivery_worker_with_lease(
            queue.clone(),
            test_delivery_config(socket_path, Duration::from_secs(1)),
            binding_rx,
            cancel.clone(),
            Duration::from_millis(50),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue.status().await.primary_consumer.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "first" } } }))
            .await
            .unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "second" } } }))
            .await
            .unwrap();

        let mut starts = Vec::new();
        for _ in 0..3 {
            starts.push(
                tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(starts, ["dione-0", "dione-0", "dione-1"]);
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue.status().await.queued != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        cancel.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn transient_rejection_retries_same_event_before_later_events() {
        let dir = TempDir::new().unwrap();
        let state_path = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let rejected_first = Arc::new(AtomicBool::new(false));
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let server = tokio::spawn({
            let rejected_first = rejected_first.clone();
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut websocket = accept_async(stream).await.unwrap();
                    while let Some(message) = websocket.next().await {
                        let Ok(Message::Text(text)) = message else {
                            break;
                        };
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let Some(id) = request.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let method = request["method"].as_str().unwrap();
                        let response = if method == "thread/read" {
                            json!({ "id": id, "result": { "thread": { "status": { "type": "idle" }, "turns": [] } } })
                        } else if method == "turn/start" {
                            started_tx
                                .send(
                                    request["params"]["clientUserMessageId"]
                                        .as_str()
                                        .unwrap()
                                        .to_owned(),
                                )
                                .unwrap();
                            if !rejected_first.swap(true, Ordering::SeqCst) {
                                json!({ "id": id, "error": { "message": "transient" } })
                            } else {
                                json!({ "id": id, "result": {} })
                            }
                        } else {
                            json!({ "id": id, "result": {} })
                        };
                        websocket
                            .send(Message::Text(response.to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let queue = CodexEventQueue::load(&state_path).unwrap();
        queue
            .bind_live_thread(Some(CodexThreadId::parse("thread-reject").unwrap()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let (_binding_tx, binding_rx) =
            tokio::sync::watch::channel(Some(CodexThreadId::parse("thread-reject").unwrap()));
        let worker = tokio::spawn(run_delivery_worker(
            queue.clone(),
            test_delivery_config(socket_path, Duration::from_secs(1)),
            binding_rx,
            cancel.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue.status().await.primary_consumer.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "first" } } }))
            .await
            .unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "second" } } }))
            .await
            .unwrap();

        let mut starts = Vec::new();
        for _ in 0..3 {
            starts.push(
                tokio::time::timeout(Duration::from_secs(5), started_rx.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        assert_eq!(starts, ["dione-0", "dione-0", "dione-1"]);

        cancel.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn stale_acknowledgement_token_does_not_retry_forever() {
        let dir = TempDir::new().unwrap();
        let state_path = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let queue = CodexEventQueue::load(&state_path).unwrap();
        let thread_id = CodexThreadId::parse("thread-stale-ack").unwrap();
        queue
            .bind_live_thread(Some(thread_id.clone()))
            .await
            .unwrap();
        let consumer_id = queue.register_live_consumer().await.unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "stale" } } }))
            .await
            .unwrap();
        let stale = queue
            .next_live_event(&consumer_id, &thread_id, Duration::ZERO, Duration::ZERO)
            .await
            .unwrap()
            .unwrap();
        let current = queue
            .next_live_event(
                &consumer_id,
                &thread_id,
                Duration::ZERO,
                Duration::from_secs(1),
            )
            .await
            .unwrap()
            .unwrap();
        assert_ne!(stale.delivery_token, current.delivery_token);

        tokio::time::timeout(
            Duration::from_millis(100),
            acknowledge_with_retry(&queue, &consumer_id, &stale, &CancellationToken::new()),
        )
        .await
        .unwrap()
        .unwrap();
        queue
            .acknowledge(&consumer_id, &current.delivery_token)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn thread_rebind_interrupts_retry_backoff() {
        let cancel = CancellationToken::new();
        let (binding_tx, mut binding_rx) = tokio::sync::watch::channel(Some(
            CodexThreadId::parse("thread-before-rebind").unwrap(),
        ));
        let wait = wait_to_retry_or_rebind(&cancel, &mut binding_rx, Duration::from_secs(30));
        tokio::pin!(wait);

        binding_tx
            .send(Some(CodexThreadId::parse("thread-after-rebind").unwrap()))
            .unwrap();
        let outcome = tokio::time::timeout(Duration::from_millis(100), wait)
            .await
            .unwrap();
        assert!(matches!(outcome, RetryWait::BindingChanged));
    }

    #[tokio::test]
    async fn live_app_server_probe_when_thread_is_configured() {
        let Ok(thread_id) = env::var("DIONE_LIVE_TEST_THREAD_ID") else {
            return;
        };
        let config = CodexDeliveryConfig::resolve(None).unwrap();
        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse(&thread_id).unwrap())
                .await
                .unwrap();
        let result = client
            .request(
                "thread/read",
                json!({ "threadId": thread_id, "includeTurns": false }),
            )
            .await
            .unwrap();
        assert_eq!(result["thread"]["id"], thread_id);
    }

    fn test_event() -> LeasedEvent {
        LeasedEvent {
            event_id: crate::codex::EventId::new(99),
            delivery_token: DeliveryToken::parse("token-test").unwrap(),
            lease_expires_at: chrono::Utc::now(),
            consumer_id: ConsumerId::parse("consumer-test").unwrap(),
            event: json!({ "params": { "content": "test event" } }),
            attention: None,
        }
    }

    #[test]
    fn preamble_1025_bytes_clamped_to_exactly_1024() {
        use crate::config::{MAX_PREAMBLE_BYTES, PreambleTemplate};

        let oversized = "a".repeat(1025);
        assert_eq!(oversized.len(), 1025);

        let template = PreambleTemplate::new(oversized);
        assert_eq!(template.as_str().len(), MAX_PREAMBLE_BYTES);
        assert_eq!(template.as_str(), "a".repeat(1024));
    }

    #[test]
    fn preamble_truncation_respects_multibyte_char_boundary() {
        use crate::config::{MAX_PREAMBLE_BYTES, PreambleTemplate};

        // Build a string of 3-byte characters (e.g. U+2603 SNOWMAN = 0xE2 0x98 0x83)
        // that would split a multibyte char at the 1024-byte boundary.
        // 341 snowmen = 1023 bytes, 342 snowmen = 1026 bytes.
        let snowman = "\u{2603}";
        assert_eq!(snowman.len(), 3);
        let oversized = snowman.repeat(342);
        assert_eq!(oversized.len(), 1026);
        assert!(oversized.len() > MAX_PREAMBLE_BYTES);

        let template = PreambleTemplate::new(oversized);
        // Must truncate to 341 snowmen (1023 bytes) — not 1024 which would
        // split the 342nd snowman's 3-byte encoding.
        assert_eq!(template.as_str().len(), 1023);
        assert_eq!(template.as_str(), snowman.repeat(341));
    }

    #[test]
    fn preamble_template_toml_deserialize_clamps() {
        use crate::config::DeliveryConfig;

        let oversized = "b".repeat(2048);
        let toml_str = format!("preamble_template = \"{oversized}\"\npreamble_mode = \"always\"\n");
        let config: DeliveryConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config.preamble_template.as_str().len(), 1024);
        assert_eq!(config.preamble_template.as_str(), "b".repeat(1024));
    }

    #[test]
    fn oversized_1mb_preamble_template_is_clamped() {
        use crate::config::{MAX_PREAMBLE_BYTES, PreambleTemplate};

        let phrase = "I will remember to length-bound my strings. ";
        let repeats = (1024 * 1024) / phrase.len() + 1;
        let oversized = phrase.repeat(repeats);
        assert!(oversized.len() >= 1024 * 1024);

        let template = PreambleTemplate::new(oversized);
        assert_eq!(template.as_str().len(), MAX_PREAMBLE_BYTES);
    }

    #[test]
    fn event_input_without_preamble_equals_raw_event() {
        let event = test_event();
        let result = event_input(&event, None);
        let text = result[0]["text"].as_str().unwrap();
        let expected = event.event.to_string();
        assert_eq!(text, expected);
    }

    #[tokio::test]
    async fn resolve_preamble_never_mode_returns_none() {
        use crate::config::PreambleTemplate;

        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                websocket
                    .send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                    .await
                    .unwrap();
                if request["method"] == "thread/resume" {
                    break;
                }
            }
            std::future::pending::<()>().await;
        });

        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.preamble_mode = crate::config::PreambleMode::Never;
        config.preamble_template = PreambleTemplate::new("should never appear");

        let client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-never").unwrap())
                .await
                .unwrap();

        assert_eq!(client.resolve_preamble(false), None);
        assert_eq!(client.resolve_preamble(true), None);
        server.abort();
    }

    #[tokio::test]
    async fn resolve_preamble_first_mode_emits_once() {
        use crate::config::PreambleTemplate;

        let dir = TempDir::new().unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut websocket = accept_async(stream).await.unwrap();
            while let Some(message) = websocket.next().await {
                let Message::Text(text) = message.unwrap() else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                websocket
                    .send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                    .await
                    .unwrap();
                if request["method"] == "thread/resume" {
                    break;
                }
            }
            std::future::pending::<()>().await;
        });

        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.preamble_mode = crate::config::PreambleMode::First;
        config.preamble_template = PreambleTemplate::new("hello");

        let client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-first").unwrap())
                .await
                .unwrap();

        let first = client.resolve_preamble(false);
        let second = client.resolve_preamble(true);
        assert_eq!(first.as_deref(), Some("hello"));
        assert_eq!(second, None);
        server.abort();
    }

    /// Delivers one event, forces a peer reconnect (simulating a WebSocket
    /// drop), then delivers a second event on the same binding and asserts
    /// the second input omits the preamble.
    #[tokio::test]
    async fn first_mode_preamble_survives_reconnect() {
        use crate::config::PreambleTemplate;

        let dir = TempDir::new().unwrap();
        let state_path = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let socket_path = Utf8PathBuf::from_path_buf(dir.path().join("app-server.sock")).unwrap();
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
        let server = tokio::spawn({
            let reset_first = Arc::new(AtomicBool::new(false));
            async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut websocket = accept_async(stream).await.unwrap();
                    while let Some(message) = websocket.next().await {
                        let Ok(Message::Text(text)) = message else {
                            break;
                        };
                        if text.is_empty() {
                            continue;
                        }
                        let request: Value = serde_json::from_str(&text).unwrap();
                        let Some(id) = request.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let method = request["method"].as_str().unwrap();
                        if method == "turn/start" {
                            // Capture the input text for assertions.
                            let input_text = request["params"]["input"][0]["text"]
                                .as_str()
                                .unwrap()
                                .to_owned();
                            input_tx.send(input_text).unwrap();
                            if !reset_first.swap(true, Ordering::SeqCst) {
                                // First delivery: accept, then drop the
                                // connection to force a reconnect.
                                websocket
                                    .send(Message::Text(
                                        json!({ "id": id, "result": {} }).to_string(),
                                    ))
                                    .await
                                    .unwrap();
                                break;
                            }
                        }
                        let response = if method == "thread/read" {
                            json!({ "id": id, "result": { "thread": { "status": { "type": "idle" }, "turns": [] } } })
                        } else {
                            json!({ "id": id, "result": {} })
                        };
                        websocket
                            .send(Message::Text(response.to_string()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let queue = CodexEventQueue::load(&state_path).unwrap();
        let thread_id = CodexThreadId::parse("thread-reconnect-preamble").unwrap();
        queue
            .bind_live_thread(Some(thread_id.clone()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let (_binding_tx, binding_rx) = tokio::sync::watch::channel(Some(thread_id));

        let mut config = test_delivery_config(socket_path, Duration::from_secs(1));
        config.preamble_mode = crate::config::PreambleMode::First;
        config.preamble_template = PreambleTemplate::new("preamble-text");

        let worker = tokio::spawn(run_delivery_worker_with_lease(
            queue.clone(),
            config,
            binding_rx,
            cancel.clone(),
            Duration::from_secs(5),
        ));

        // Wait for the consumer to register.
        tokio::time::timeout(Duration::from_secs(1), async {
            while queue.status().await.primary_consumer.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // Enqueue two events — the first will be delivered, then the peer
        // drops, forcing reconnect for the second.
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "evt-1" } } }))
            .await
            .unwrap();
        queue
            .enqueue(json!({ "params": { "meta": { "message_id": "evt-2" } } }))
            .await
            .unwrap();

        // Collect the two delivered inputs.
        let first_input = tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let second_input = tokio::time::timeout(Duration::from_secs(5), input_rx.recv())
            .await
            .unwrap()
            .unwrap();

        // First delivery must include the preamble.
        assert!(
            first_input.contains("preamble-text"),
            "first delivery should include preamble, got: {first_input}"
        );
        // Second delivery (after reconnect) must NOT include the preamble.
        assert!(
            !second_input.contains("preamble-text"),
            "second delivery after reconnect should omit preamble, got: {second_input}"
        );

        cancel.cancel();
        worker.await.unwrap().unwrap();
        server.abort();
    }
}
