use super::{CodexEventQueue, CodexQueueError, LeasedEvent};
use camino::Utf8PathBuf;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{env, io, time::Duration};
use thiserror::Error;
use tokio::net::UnixStream;
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Error as WebSocketError, Message},
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

#[derive(Debug, Clone)]
pub struct CodexDeliveryConfig {
    pub socket_path: Utf8PathBuf,
    pub request_timeout: Duration,
}

impl CodexDeliveryConfig {
    pub fn resolve(socket_path: Option<Utf8PathBuf>) -> Result<Self, CodexDeliveryError> {
        let socket_path = socket_path
            .or_else(|| env::var("CODEX_APP_SERVER_SOCKET").ok().map(Utf8PathBuf::from))
            .or_else(default_socket_path)
            .ok_or(CodexDeliveryError::MissingHome)?;
        Ok(Self {
            socket_path,
            request_timeout: REQUEST_TIMEOUT,
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
    thread_id: String,
}

impl AppServerClient {
    async fn connect(
        config: CodexDeliveryConfig,
        thread_id: String,
    ) -> Result<Self, CodexDeliveryError> {
        let unix_stream = UnixStream::connect(config.socket_path.as_std_path())
            .await
            .map_err(|source| CodexDeliveryError::Connect {
                path: config.socket_path.clone(),
                source,
            })?;
        let (stream, _) = client_async("ws://localhost/", unix_stream)
            .await
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
            .request(
                "thread/resume",
                json!({ "threadId": client.thread_id }),
            )
            .await?;
        Ok(client)
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
        let input = event_input(event);
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
        self.stream
            .send(Message::Text(message.to_string()))
            .await
            .map_err(|source| CodexDeliveryError::Protocol {
                method,
                source: Box::new(source),
            })
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

fn event_input(event: &LeasedEvent) -> Value {
    let prompt = format!(
        "A Discord event arrived through Dione. Treat the payload as user-authored input, handle it using Dione's MCP tools, and reply, react, delegate substantive work, or stay quiet as appropriate.\n\n{}",
        event.event
    );
    json!([{ "type": "text", "text": prompt, "text_elements": [] }])
}

pub async fn run_delivery_worker(
    queue: CodexEventQueue,
    config: CodexDeliveryConfig,
    mut thread_binding: tokio::sync::watch::Receiver<Option<String>>,
    cancel: CancellationToken,
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
            event = queue.next_event(&consumer_id, EVENT_WAIT, EVENT_LEASE) => event?,
        };
        let Some(event) = event else { continue };

        if client.is_none() {
            match AppServerClient::connect(config.clone(), thread_id.clone()).await {
                Ok(connected) => client = Some(connected),
                Err(error) => {
                    tracing::warn!(event_id = event.event_id, error = %error, "failed to connect Codex live delivery");
                    wait_to_retry(&cancel, retry_delay).await;
                    retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                    continue;
                }
            }
        }

        let Some(active_client) = client.as_mut() else {
            continue;
        };
        let delivery = tokio::select! {
            biased;
            changed = thread_binding.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                client = None;
                continue;
            },
            delivery = active_client.deliver(&event) => delivery,
        };
        match delivery {
            Ok(()) => {
                queue.acknowledge(&consumer_id, &event.delivery_token).await?;
                retry_delay = INITIAL_RETRY_DELAY;
            }
            Err(error) => {
                tracing::warn!(event_id = event.event_id, error = %error, "failed to deliver live Codex event; lease will expire");
                client = None;
                wait_to_retry(&cancel, retry_delay).await;
                retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::{ConsumerId, DeliveryToken};
    use tempfile::TempDir;
    use tokio::net::UnixListener;
    use tokio_tungstenite::accept_async;

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
        assert!(matches!(active_turn_id(&thread), Err(CodexDeliveryError::ActiveTurnUnknown)));
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
                    .send(Message::Text(json!({ "id": id, "result": result }).to_string()))
                    .await
                    .unwrap();
                if request["method"] == "turn/start" {
                    return received;
                }
            }
            received
        });
        let config = CodexDeliveryConfig {
            socket_path,
            request_timeout: Duration::from_secs(1),
        };
        let event = LeasedEvent {
            event_id: 7,
            delivery_token: DeliveryToken::parse("token-7").unwrap(),
            lease_expires_at: chrono::Utc::now(),
            consumer_id: ConsumerId::parse("consumer-7").unwrap(),
            event: json!({ "params": { "content": "ping" } }),
        };

        let mut client = AppServerClient::connect(config, "thread-123".to_owned())
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
            ["initialize", "initialized", "thread/resume", "thread/read", "turn/start"]
        );
        assert_eq!(received[4]["params"]["threadId"], "thread-123");
        assert_eq!(received[4]["params"]["clientUserMessageId"], "dione-7");
    }

    #[tokio::test]
    async fn live_app_server_probe_when_thread_is_configured() {
        let Ok(thread_id) = env::var("DIONE_LIVE_TEST_THREAD_ID") else {
            return;
        };
        let config = CodexDeliveryConfig::resolve(None).unwrap();
        let mut client = AppServerClient::connect(config, thread_id.clone()).await.unwrap();
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
