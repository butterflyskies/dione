use super::{CodexEventQueue, CodexQueueError, CodexThreadId, LeasedEvent};
use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{env, io, time::Duration};
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
// One delivery can spend up to four request timeouts connecting/resuming,
// reading the thread, and starting or steering a turn. Keep the lease well
// beyond that bound so a successfully accepted turn can still be acknowledged.
const EVENT_LEASE: Duration = Duration::from_secs(5 * 60);
const MAX_WEBSOCKET_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CodexDeliveryConfig {
    pub socket_path: Utf8PathBuf,
    pub request_timeout: Duration,
    pub preamble_mode: crate::config::PreambleMode,
    pub preamble_template: String,
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
    #[error("Codex thread is active but its current turn id is unavailable")]
    ActiveTurnUnknown,
    #[error(transparent)]
    Queue(#[from] CodexQueueError),
}

struct AppServerClient {
    stream: WebSocketStream<UnixStream>,
    next_request_id: u64,
    config: CodexDeliveryConfig,
    thread_id: CodexThreadId,
    preamble_sent: bool,
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
            preamble_sent: false,
        };
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "dione",
                        "title": "Dione",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        client
            .send(
                "initialized",
                json!({ "method": "initialized", "params": {} }),
            )
            .await?;
        client
            .request("thread/resume", json!({ "threadId": client.thread_id }))
            .await?;
        Ok(client)
    }

    fn resolve_preamble(&mut self) -> Option<String> {
        use crate::config::PreambleMode;
        match self.config.preamble_mode {
            PreambleMode::Always => Some(self.config.preamble_template.clone()),
            PreambleMode::First => {
                if self.preamble_sent {
                    None
                } else {
                    self.preamble_sent = true;
                    Some(self.config.preamble_template.clone())
                }
            }
            PreambleMode::Never => None,
        }
    }

    async fn deliver(&mut self, event: &LeasedEvent) -> Result<(), CodexDeliveryError> {
        let result = self
            .request(
                "thread/read",
                json!({
                    "threadId": self.thread_id,
                    "includeTurns": true
                }),
            )
            .await?;
        let thread = result.get("thread").cloned().unwrap_or(Value::Null);
        let active_turn_id = active_turn_id(&thread)?;
        let preamble = self.resolve_preamble();
        let input = event_input(event, preamble.as_deref());
        let client_message_id = format!("dione-{}", event.event_id);
        if let Some(turn_id) = active_turn_id {
            self.request(
                "turn/steer",
                json!({
                    "threadId": self.thread_id,
                    "expectedTurnId": turn_id,
                    "clientUserMessageId": client_message_id,
                    "input": input
                }),
            )
            .await?;
        } else {
            self.request(
                "turn/start",
                json!({
                    "threadId": self.thread_id,
                    "clientUserMessageId": client_message_id,
                    "input": input
                }),
            )
            .await?;
        }
        Ok(())
    }

    async fn request(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<Value, CodexDeliveryError> {
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
                    return Err(CodexDeliveryError::Disconnected { method });
                };
                match message.map_err(|source| CodexDeliveryError::Protocol {
                    method,
                    source: Box::new(source),
                })? {
                    Message::Text(text) => {
                        let message: Value = serde_json::from_str(&text).map_err(|source| {
                            CodexDeliveryError::Rejected {
                                method,
                                message: format!("invalid JSON response: {source}"),
                            }
                        })?;
                        if message.get("id").and_then(Value::as_u64) != Some(request_id) {
                            continue;
                        }
                        if let Some(error) = message.get("error") {
                            return Err(CodexDeliveryError::Rejected {
                                method,
                                message: error.to_string(),
                            });
                        }
                        return Ok(message.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Message::Ping(payload) => self
                        .stream
                        .send(Message::Pong(payload))
                        .await
                        .map_err(|source| CodexDeliveryError::Protocol {
                            method,
                            source: Box::new(source),
                        })?,
                    Message::Close(_) => return Err(CodexDeliveryError::Disconnected { method }),
                    Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        })
        .await
        .map_err(|_| CodexDeliveryError::Timeout { method })?
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

fn websocket_config() -> WebSocketConfig {
    WebSocketConfig {
        // App-server responses such as `thread/resume` may contain an entire
        // long-running thread in one frame. Tungstenite's default frame limit
        // is 16 MiB even though its message limit is 64 MiB.
        max_message_size: Some(MAX_WEBSOCKET_MESSAGE_SIZE),
        max_frame_size: Some(MAX_WEBSOCKET_MESSAGE_SIZE),
        max_write_buffer_size: MAX_WEBSOCKET_MESSAGE_SIZE + 128 * 1024,
        ..WebSocketConfig::default()
    }
}

fn active_turn_id(thread: &Value) -> Result<Option<String>, CodexDeliveryError> {
    let active = thread.pointer("/status/type").and_then(Value::as_str) == Some("active");
    let turn_id = thread
        .get("turns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .rev()
        .find(|turn| turn.get("status").and_then(Value::as_str) == Some("inProgress"))
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if active && turn_id.is_none() {
        return Err(CodexDeliveryError::ActiveTurnUnknown);
    }
    Ok(turn_id)
}

fn event_input(event: &LeasedEvent, preamble: Option<&str>) -> Value {
    let prompt = match preamble {
        Some(pre) => format!("{pre}\n\n{}", event.event),
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
                                retry_delay = INITIAL_RETRY_DELAY;
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
            let delivery = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                changed = thread_binding.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    client = None;
                    break;
                },
                delivery = active_client.deliver(&event) => delivery,
            };
            match delivery {
                Ok(()) => {
                    acknowledge_with_retry(&queue, &consumer_id, &event, &cancel).await?;
                    retry_delay = INITIAL_RETRY_DELAY;
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
                            retry_delay = INITIAL_RETRY_DELAY;
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
    use crate::codex::{ConsumerId, DeliveryToken};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
        }
    }

    #[test]
    fn finds_in_progress_turn() {
        let thread = json!({
            "status": { "type": "active", "activeFlags": [] },
            "turns": [
                { "id": "done", "status": "completed" },
                { "id": "live", "status": "inProgress" }
            ]
        });
        assert_eq!(active_turn_id(&thread).unwrap().as_deref(), Some("live"));
    }

    #[test]
    fn refuses_active_thread_without_turn_id() {
        let thread = json!({ "status": { "type": "active" }, "turns": [] });
        assert!(matches!(
            active_turn_id(&thread),
            Err(CodexDeliveryError::ActiveTurnUnknown)
        ));
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
        assert!(MAX_WEBSOCKET_MESSAGE_SIZE > 16 * 1024 * 1024);
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
                let Some(id) = request.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                websocket
                    .send(Message::Text(json!({ "id": id, "result": {} }).to_string()))
                    .await
                    .unwrap();
                if request["method"] == "thread/resume" {
                    std::future::pending::<()>().await;
                }
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
                if request["method"] == "thread/resume" {
                    break;
                }
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
        };

        let mut client =
            AppServerClient::connect(config, CodexThreadId::parse("thread-123").unwrap())
                .await
                .unwrap();
        client.deliver(&event).await.unwrap();

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
                "thread/resume",
                "thread/read",
                "turn/start"
            ]
        );
        assert_eq!(received[4]["params"]["threadId"], "thread-123");
        assert_eq!(received[4]["params"]["clientUserMessageId"], "dione-7");
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
}
