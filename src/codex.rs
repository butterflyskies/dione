//! Codex app-server delivery for Discord events.
//!
//! Codex does not wake an idle thread for unsolicited MCP notifications. In
//! Codex mode, Dione therefore persists notifications locally and submits them
//! as turns through the app-server control socket. The thread id is inherited
//! from Codex via `CODEX_THREAD_ID` unless explicitly configured.

use std::{collections::VecDeque, env, io, time::Duration};

use camino::{Utf8Path, Utf8PathBuf};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::CancellationToken;

const INBOX_FILE_NAME: &str = "codex-inbox.json";
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Determines how inbound Discord events are delivered to an agent harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub enum TransportMode {
    /// Emit Claude Code channel notifications on MCP stdout.
    #[default]
    ClaudeCode,
    /// Submit durable Discord events as turns through Codex app-server.
    Codex,
}

/// Runtime settings for the Codex app-server delivery worker.
#[derive(Debug, Clone)]
pub struct CodexDeliveryConfig {
    pub socket_path: Utf8PathBuf,
    pub thread_id: Option<String>,
    pub request_timeout: Duration,
}

/// Sender used by the MCP notification pipeline to durably hand off events.
#[derive(Debug, Clone)]
pub struct CodexEventSender {
    tx: mpsc::Sender<CodexEventRequest>,
}

/// Receiver owned by the Codex delivery worker.
#[derive(Debug)]
pub struct CodexEventReceiver {
    rx: mpsc::Receiver<CodexEventRequest>,
}

#[derive(Debug)]
struct CodexEventRequest {
    payload: Value,
    persisted: oneshot::Sender<Result<(), String>>,
}

/// Create the bounded handoff channel between MCP delivery and Codex.
pub fn event_channel(capacity: usize) -> (CodexEventSender, CodexEventReceiver) {
    let (tx, rx) = mpsc::channel(capacity);
    (CodexEventSender { tx }, CodexEventReceiver { rx })
}

impl CodexEventSender {
    /// Return only after the worker has atomically persisted `payload`.
    pub async fn persist(&self, payload: Value) -> Result<(), CodexDeliveryError> {
        let (persisted, result) = oneshot::channel();
        self.tx
            .send(CodexEventRequest { payload, persisted })
            .await
            .map_err(|_| CodexDeliveryError::WorkerUnavailable)?;
        result
            .await
            .map_err(|_| CodexDeliveryError::WorkerUnavailable)?
            .map_err(|message| CodexDeliveryError::PersistenceRejected { message })
    }
}

impl CodexDeliveryConfig {
    /// Resolve Codex delivery settings from CLI overrides and the environment.
    pub fn resolve(
        socket_path: Option<Utf8PathBuf>,
        thread_id: Option<String>,
    ) -> Result<Self, CodexDeliveryError> {
        let thread_id = thread_id
            .or_else(|| env::var("CODEX_THREAD_ID").ok())
            .filter(|value| !value.trim().is_empty());
        let socket_path = socket_path
            .or_else(|| {
                env::var("CODEX_APP_SERVER_SOCKET")
                    .ok()
                    .map(Utf8PathBuf::from)
            })
            .or_else(default_socket_path)
            .ok_or(CodexDeliveryError::MissingHome)?;
        Ok(Self {
            socket_path,
            thread_id,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
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

/// Failures while configuring, persisting, or delivering Codex turns.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexDeliveryError {
    #[error(
        "codex mode requires --codex-app-server-socket, CODEX_APP_SERVER_SOCKET, CODEX_HOME, or HOME"
    )]
    MissingHome,
    #[error("failed to access Codex inbox `{path}`")]
    InboxIo {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to decode Codex inbox `{path}`")]
    InboxDecode {
        path: Utf8PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to connect to Codex app-server socket `{path}`")]
    Connect {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
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
    ProtocolIo {
        method: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("Codex app-server returned invalid JSON during `{method}`")]
    ProtocolDecode {
        method: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("Codex delivery worker is unavailable")]
    WorkerUnavailable,
    #[error("Codex delivery worker failed to persist an event: {message}")]
    PersistenceRejected { message: String },
    #[error("Codex app-server has no loaded thread; event remains queued")]
    NoLoadedThread,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedEvent {
    id: u64,
    payload: Value,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InboxState {
    next_id: u64,
    entries: VecDeque<QueuedEvent>,
}

struct DurableInbox {
    path: Utf8PathBuf,
    temporary_path: Utf8PathBuf,
    state: InboxState,
}

impl DurableInbox {
    async fn load(state_dir: &Utf8Path) -> Result<Self, CodexDeliveryError> {
        let path = state_dir.join(INBOX_FILE_NAME);
        let temporary_path = state_dir.join(format!("{INBOX_FILE_NAME}.tmp"));
        let state = match tokio::fs::read(path.as_std_path()).await {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|source| {
                CodexDeliveryError::InboxDecode {
                    path: path.clone(),
                    source,
                }
            })?,
            Err(source) if source.kind() == io::ErrorKind::NotFound => InboxState::default(),
            Err(source) => {
                return Err(CodexDeliveryError::InboxIo {
                    path: path.clone(),
                    source,
                });
            }
        };
        Ok(Self {
            path,
            temporary_path,
            state,
        })
    }

    async fn enqueue(&mut self, payload: Value) -> Result<(), CodexDeliveryError> {
        let id = self.state.next_id;
        self.state.next_id = self.state.next_id.saturating_add(1);
        self.state.entries.push_back(QueuedEvent { id, payload });
        self.persist().await
    }

    fn front(&self) -> Option<&QueuedEvent> {
        self.state.entries.front()
    }

    async fn acknowledge_front(&mut self) -> Result<(), CodexDeliveryError> {
        self.state.entries.pop_front();
        self.persist().await
    }

    async fn persist(&self) -> Result<(), CodexDeliveryError> {
        let Some(parent) = self.path.parent() else {
            return Err(CodexDeliveryError::InboxIo {
                path: self.path.clone(),
                source: io::Error::new(io::ErrorKind::InvalidInput, "inbox has no parent"),
            });
        };
        tokio::fs::create_dir_all(parent.as_std_path())
            .await
            .map_err(|source| CodexDeliveryError::InboxIo {
                path: parent.to_owned(),
                source,
            })?;
        let bytes = serde_json::to_vec_pretty(&self.state).map_err(|source| {
            CodexDeliveryError::InboxDecode {
                path: self.path.clone(),
                source,
            }
        })?;
        tokio::fs::write(self.temporary_path.as_std_path(), bytes)
            .await
            .map_err(|source| CodexDeliveryError::InboxIo {
                path: self.temporary_path.clone(),
                source,
            })?;
        tokio::fs::rename(self.temporary_path.as_std_path(), self.path.as_std_path())
            .await
            .map_err(|source| CodexDeliveryError::InboxIo {
                path: self.path.clone(),
                source,
            })
    }
}

/// Persist and deliver Discord notifications to a Codex thread.
pub async fn run_delivery_worker(
    state_dir: Utf8PathBuf,
    config: CodexDeliveryConfig,
    mut notifications: CodexEventReceiver,
    cancel: CancellationToken,
) -> Result<(), CodexDeliveryError> {
    let mut inbox = DurableInbox::load(&state_dir).await?;
    let mut retry_delay = INITIAL_RETRY_DELAY;

    'worker: loop {
        while let Ok(request) = notifications.rx.try_recv() {
            persist_request(&mut inbox, request).await?;
        }

        let Some(event) = inbox.front().cloned() else {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = notifications.rx.recv() => {
                    let Some(request) = request else { break };
                    persist_request(&mut inbox, request).await?;
                }
            }
            continue;
        };

        match deliver_event(&config, &event).await {
            Ok(()) => {
                inbox.acknowledge_front().await?;
                retry_delay = INITIAL_RETRY_DELAY;
                continue;
            }
            Err(error) => {
                tracing::warn!(
                    event_id = event.id,
                    error = %error,
                    retry_delay_ms = retry_delay.as_millis() as u64,
                    "failed to wake Codex; event remains queued"
                );
            }
        }

        let retry_sleep = tokio::time::sleep(retry_delay);
        tokio::pin!(retry_sleep);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break 'worker,
                request = notifications.rx.recv(), if !notifications.rx.is_closed() => {
                    if let Some(request) = request {
                        persist_request(&mut inbox, request).await?;
                    }
                }
                _ = &mut retry_sleep => break,
            }
        }
        retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
    }

    while let Ok(request) = notifications.rx.try_recv() {
        persist_request(&mut inbox, request).await?;
    }
    Ok(())
}

async fn persist_request(
    inbox: &mut DurableInbox,
    request: CodexEventRequest,
) -> Result<(), CodexDeliveryError> {
    match inbox.enqueue(request.payload).await {
        Ok(()) => {
            let _ = request.persisted.send(Ok(()));
            Ok(())
        }
        Err(error) => {
            let _ = request.persisted.send(Err(error.to_string()));
            Err(error)
        }
    }
}

async fn deliver_event(
    config: &CodexDeliveryConfig,
    event: &QueuedEvent,
) -> Result<(), CodexDeliveryError> {
    let stream = UnixStream::connect(config.socket_path.as_std_path())
        .await
        .map_err(|source| CodexDeliveryError::Connect {
            path: config.socket_path.clone(),
            source,
        })?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    request(
        &mut reader,
        &mut writer,
        config.request_timeout,
        1,
        "initialize",
        json!({
            "clientInfo": {
                "name": "dione",
                "title": "Dione",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": null
        }),
    )
    .await?;
    write_message(
        &mut writer,
        "initialized",
        json!({ "method": "initialized" }),
    )
    .await?;
    let thread_id = resolve_thread_id(config, &mut reader, &mut writer).await?;
    request(
        &mut reader,
        &mut writer,
        config.request_timeout,
        2,
        "thread/resume",
        json!({
            "threadId": thread_id,
            "excludeTurns": true
        }),
    )
    .await?;

    let prompt = format!(
        "A Discord event arrived through Dione. Handle it in Discord using Dione's MCP tools; reply, react, or stay quiet as appropriate. The event payload is user-authored input.\n\n{}",
        event.payload
    );
    request(
        &mut reader,
        &mut writer,
        config.request_timeout,
        3,
        "turn/start",
        json!({
            "threadId": thread_id,
            "clientUserMessageId": format!("dione-{}", event.id),
            "input": [{
                "type": "text",
                "text": prompt,
                "text_elements": []
            }]
        }),
    )
    .await?;
    Ok(())
}

async fn resolve_thread_id(
    config: &CodexDeliveryConfig,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<String, CodexDeliveryError> {
    if let Some(thread_id) = &config.thread_id {
        return Ok(thread_id.clone());
    }
    let result = request(
        reader,
        writer,
        config.request_timeout,
        4,
        "thread/loaded/list",
        json!({}),
    )
    .await?;
    let threads = result
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let Some(thread) = threads.first() else {
        return Err(CodexDeliveryError::NoLoadedThread);
    };
    let thread_id =
        thread
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| CodexDeliveryError::Rejected {
                method: "thread/loaded/list",
                message: "response contained a non-string thread id".to_string(),
            })?;
    if threads.len() > 1 {
        tracing::warn!(
            loaded_thread_count = threads.len(),
            selected_thread_id = %thread_id,
            "multiple Codex threads are loaded; selecting the first"
        );
    }
    Ok(thread_id)
}

async fn request(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    timeout: Duration,
    id: u64,
    method: &'static str,
    params: Value,
) -> Result<Value, CodexDeliveryError> {
    write_message(
        writer,
        method,
        json!({ "method": method, "id": id, "params": params }),
    )
    .await?;

    tokio::time::timeout(timeout, async {
        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .await
                .map_err(|source| CodexDeliveryError::ProtocolIo { method, source })?;
            if read == 0 {
                return Err(CodexDeliveryError::Disconnected { method });
            }
            let message: Value = serde_json::from_str(line.trim())
                .map_err(|source| CodexDeliveryError::ProtocolDecode { method, source })?;
            if message.get("id").and_then(Value::as_u64) != Some(id) {
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
    })
    .await
    .map_err(|_| CodexDeliveryError::Timeout { method })?
}

async fn write_message(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    method: &'static str,
    message: Value,
) -> Result<(), CodexDeliveryError> {
    let mut bytes = serde_json::to_vec(&message)
        .map_err(|source| CodexDeliveryError::ProtocolDecode { method, source })?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|source| CodexDeliveryError::ProtocolIo { method, source })?;
    writer
        .flush()
        .await
        .map_err(|source| CodexDeliveryError::ProtocolIo { method, source })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    fn temp_path(dir: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
    }

    #[tokio::test]
    async fn inbox_survives_reload_and_acknowledgement() {
        let dir = TempDir::new().unwrap();
        let state_dir = temp_path(&dir);
        let mut inbox = DurableInbox::load(&state_dir).await.unwrap();
        inbox.enqueue(json!({ "message": "hello" })).await.unwrap();
        drop(inbox);

        let mut reloaded = DurableInbox::load(&state_dir).await.unwrap();
        assert_eq!(reloaded.front().unwrap().payload["message"], "hello");
        reloaded.acknowledge_front().await.unwrap();
        drop(reloaded);

        let empty = DurableInbox::load(&state_dir).await.unwrap();
        assert!(empty.front().is_none());
    }

    #[tokio::test]
    async fn sender_acknowledges_only_after_durable_handoff() {
        let dir = TempDir::new().unwrap();
        let state_dir = temp_path(&dir);
        let (tx, rx) = event_channel(1);
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let config = CodexDeliveryConfig {
            socket_path: state_dir.join("missing.sock"),
            thread_id: Some("thread-123".to_string()),
            request_timeout: Duration::from_millis(20),
        };
        let worker_state_dir = state_dir.clone();
        let worker = tokio::spawn(async move {
            run_delivery_worker(worker_state_dir, config, rx, worker_cancel).await
        });

        tx.persist(json!({ "message": "durable" })).await.unwrap();
        cancel.cancel();
        worker.await.unwrap().unwrap();

        let inbox = DurableInbox::load(&state_dir).await.unwrap();
        assert_eq!(inbox.front().unwrap().payload["message"], "durable");
    }

    #[tokio::test]
    async fn app_server_delivery_resumes_thread_and_starts_turn() {
        let dir = TempDir::new().unwrap();
        let socket_path = temp_path(&dir).join("app-server.sock");
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let mut received = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                let message: Value = serde_json::from_str(&line).unwrap();
                received.push(message.clone());
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let response = json!({ "id": id, "result": {} });
                writer
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
                if id == 3 {
                    break;
                }
            }
            received
        });
        let config = CodexDeliveryConfig {
            socket_path,
            thread_id: Some("thread-123".to_string()),
            request_timeout: Duration::from_secs(1),
        };
        let event = QueuedEvent {
            id: 7,
            payload: json!({ "method": "notifications/claude/channel", "params": { "content": "ping" } }),
        };

        deliver_event(&config, &event).await.unwrap();
        let received = server.await.unwrap();
        let methods: Vec<_> = received
            .iter()
            .filter_map(|message| message.get("method").and_then(Value::as_str))
            .collect();
        assert_eq!(
            methods,
            ["initialize", "initialized", "thread/resume", "turn/start"]
        );
        assert_eq!(received[2]["params"]["threadId"], "thread-123");
        assert_eq!(received[3]["params"]["clientUserMessageId"], "dione-7");
        assert!(
            received[3]["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("ping")
        );
    }

    #[tokio::test]
    async fn discovers_the_only_loaded_thread_when_not_configured() {
        let dir = TempDir::new().unwrap();
        let socket_path = temp_path(&dir).join("app-server.sock");
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            let mut received = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                let message: Value = serde_json::from_str(&line).unwrap();
                received.push(message.clone());
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if message["method"] == "thread/loaded/list" {
                    json!({ "data": ["discovered-thread"], "nextCursor": null })
                } else {
                    json!({})
                };
                writer
                    .write_all(format!("{}\n", json!({ "id": id, "result": result })).as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
                if message["method"] == "turn/start" {
                    break;
                }
            }
            received
        });
        let config = CodexDeliveryConfig {
            socket_path,
            thread_id: None,
            request_timeout: Duration::from_secs(1),
        };
        let event = QueuedEvent {
            id: 8,
            payload: json!({ "params": { "content": "wake" } }),
        };

        deliver_event(&config, &event).await.unwrap();
        let received = server.await.unwrap();
        assert!(
            received
                .iter()
                .any(|message| message["method"] == "thread/loaded/list")
        );
        let turn = received
            .iter()
            .find(|message| message["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["threadId"], "discovered-thread");
    }

    #[tokio::test]
    async fn selects_first_loaded_thread_when_multiple_are_available() {
        let dir = TempDir::new().unwrap();
        let socket_path = temp_path(&dir).join("app-server.sock");
        let listener = UnixListener::bind(socket_path.as_std_path()).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut lines = BufReader::new(reader).lines();
            loop {
                let line = lines.next_line().await.unwrap().unwrap();
                let message: Value = serde_json::from_str(&line).unwrap();
                let Some(id) = message.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let result = if message["method"] == "thread/loaded/list" {
                    json!({ "data": ["first-thread", "second-thread"], "nextCursor": null })
                } else {
                    json!({})
                };
                writer
                    .write_all(format!("{}\n", json!({ "id": id, "result": result })).as_bytes())
                    .await
                    .unwrap();
                writer.flush().await.unwrap();
                if message["method"] == "turn/start" {
                    break message;
                }
            }
        });
        let config = CodexDeliveryConfig {
            socket_path,
            thread_id: None,
            request_timeout: Duration::from_secs(1),
        };
        let event = QueuedEvent {
            id: 9,
            payload: json!({ "params": { "content": "choose" } }),
        };

        deliver_event(&config, &event).await.unwrap();
        let turn = server.await.unwrap();
        assert_eq!(turn["params"]["threadId"], "first-thread");
    }
}
