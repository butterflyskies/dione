//! Local control path for binding a Codex session to live delivery.

use crate::codex::{CodexEventQueue, CodexQueueError, CodexThreadId};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::{Mutex, watch},
    task::JoinSet,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const CONTROL_SOCKET_FILE_NAME: &str = "codex-control.sock";
const PROTOCOL_VERSION: u8 = 1;
const MAX_FRAME_BYTES: u64 = 4 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(25);
const MAX_ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);
const INITIAL_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(25);
const MAX_CONNECT_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_CONCURRENT_CONNECTIONS: usize = 16;
const STAGING_DIRECTORY_PREFIX: &str = ".c-";
const STAGING_DIRECTORY_RANDOM_BYTES: usize = 6;
const STAGED_SOCKET_FILE_NAME: &str = "s";
type ConnectionResult = Result<Result<(), BindControlError>, tokio::time::error::Elapsed>;
type ConnectionCompletion = Result<ConnectionResult, tokio::task::JoinError>;

/// Serializes durable and live Codex thread binding updates.
#[derive(Clone)]
pub struct CodexThreadBinder {
    state: std::sync::Arc<Mutex<BinderState>>,
}

struct BinderState {
    queue: CodexEventQueue,
    live_binding: watch::Sender<Option<CodexThreadId>>,
}

impl CodexThreadBinder {
    /// Creates a binder over one durable queue and live delivery worker.
    pub fn new(queue: CodexEventQueue, live_binding: watch::Sender<Option<CodexThreadId>>) -> Self {
        Self {
            state: std::sync::Arc::new(Mutex::new(BinderState {
                queue,
                live_binding,
            })),
        }
    }

    /// Persists a binding before publishing it to the live delivery worker.
    pub async fn bind(&self, thread_id: CodexThreadId) -> Result<(), CodexThreadBindingError> {
        self.set(Some(thread_id)).await
    }

    /// Clears both the durable and live Codex thread binding.
    pub async fn clear(&self) -> Result<(), CodexThreadBindingError> {
        self.set(None).await
    }

    async fn set(&self, thread_id: Option<CodexThreadId>) -> Result<(), CodexThreadBindingError> {
        let state = self.state.lock().await;
        let previous = state.live_binding.borrow().clone();
        let durable = state.queue.live_thread_id().await;
        if previous == thread_id && durable == thread_id {
            tracing::debug!(
                thread_id = thread_id.as_ref().map(CodexThreadId::as_str),
                "Codex thread binding is already current"
            );
            return Ok(());
        }
        let clearing = thread_id.is_none();
        if let Err(source) = state.queue.bind_live_thread(thread_id.clone()).await {
            if !clearing {
                // The durable update failed, so stop live delivery from using
                // a stale previous binding. A best-effort durable clear makes
                // the same fail-closed state survive restart when storage is
                // still writable.
                let _ = state.live_binding.send(None);
                if let Err(clear_error) = state.queue.bind_live_thread(None).await {
                    tracing::error!(
                        error = %clear_error,
                        "failed to clear the durable Codex binding after a bind failure"
                    );
                }
            }
            return Err(CodexThreadBindingError::Persist(source));
        }

        if state.live_binding.send(thread_id.clone()).is_err() {
            if clearing {
                return Ok(());
            }
            return match state.queue.bind_live_thread(None).await {
                Ok(()) => Err(CodexThreadBindingError::LiveWorkerUnavailable),
                Err(source) => Err(CodexThreadBindingError::Rollback {
                    source: Box::new(source),
                }),
            };
        }
        tracing::info!(
            previous_thread_id = previous.as_ref().map(CodexThreadId::as_str),
            thread_id = thread_id.as_ref().map(CodexThreadId::as_str),
            "Codex thread binding changed"
        );
        Ok(())
    }
}

/// Failure while changing the canonical Codex thread binding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexThreadBindingError {
    #[error("failed to persist the Codex thread binding")]
    Persist(#[source] CodexQueueError),
    #[error("Codex live delivery worker is unavailable")]
    LiveWorkerUnavailable,
    #[error("Codex live delivery worker is unavailable and clearing the durable binding failed")]
    Rollback {
        #[source]
        source: Box<CodexQueueError>,
    },
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: CodexThreadId,
    hook_event_name: String,
    #[serde(rename = "source")]
    _source: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BindRequest {
    version: u8,
    operation: BindOperation,
    thread_id: CodexThreadId,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BindOperation {
    BindCodexThread,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BindResponse {
    Bound {
        version: u8,
        thread_id: CodexThreadId,
    },
    Error {
        version: u8,
        message: String,
    },
}

/// A bound local listener that accepts Codex thread-binding requests.
#[non_exhaustive]
pub struct CodexBindControlListener {
    listener: UnixListener,
    socket_path: Utf8PathBuf,
    socket_guard: SocketGuard,
    binder: CodexThreadBinder,
}

impl CodexBindControlListener {
    /// Binds and protects the control socket before the daemon reports startup success.
    pub async fn bind(
        state_dir: &Utf8Path,
        binder: CodexThreadBinder,
    ) -> Result<Self, BindControlError> {
        validate_state_dir(state_dir)?;
        let socket_path = state_dir.join(CONTROL_SOCKET_FILE_NAME);
        let listener = bind_listener(&socket_path).await?;
        let socket_guard = match SocketGuard::new(socket_path.clone()) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = std::fs::remove_file(socket_path.as_std_path());
                return Err(error);
            }
        };
        Ok(Self {
            listener,
            socket_path,
            socket_guard,
            binder,
        })
    }

    /// Accepts one bounded request per connection until cancellation.
    pub async fn run(self, cancel: CancellationToken) -> Result<(), BindControlError> {
        let Self {
            listener,
            socket_path,
            socket_guard,
            binder,
        } = self;
        let _socket_guard = socket_guard;
        let mut connections = JoinSet::new();
        let mut retry_delay = INITIAL_ACCEPT_RETRY_DELAY;
        loop {
            if connections.len() >= MAX_CONCURRENT_CONNECTIONS {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    completed = connections.join_next() => {
                        log_connection_completion(completed);
                        continue;
                    }
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                completed = connections.join_next(), if !connections.is_empty() => {
                    log_connection_completion(completed);
                }
                accepted = listener.accept() => {
                    match accepted {
                        Ok((mut stream, _)) => {
                            retry_delay = INITIAL_ACCEPT_RETRY_DELAY;
                            let binder = binder.clone();
                            let connection_path = socket_path.clone();
                            connections.spawn(async move {
                                timeout(CONTROL_TIMEOUT, async {
                                    validate_peer(&stream)?;
                                    handle_connection(&mut stream, &binder, &connection_path).await
                                })
                                .await
                            });
                        }
                        Err(source) if is_retryable_accept_error(&source) => {
                            tracing::warn!(
                                error = %source,
                                delay_ms = retry_delay.as_millis(),
                                "transient Codex bind control accept failure"
                            );
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => break,
                                _ = sleep(retry_delay) => {}
                            }
                            retry_delay = retry_delay
                                .saturating_mul(2)
                                .min(MAX_ACCEPT_RETRY_DELAY);
                        }
                        Err(source) => {
                            return Err(BindControlError::Io {
                                action: "accept a control connection",
                                path: socket_path.clone(),
                                source,
                            });
                        }
                    }
                }
            }
        }
        while let Some(completed) = connections.join_next().await {
            log_connection_completion(Some(completed));
        }
        Ok(())
    }
}

/// Reads a Codex `SessionStart` hook payload and requires an exact daemon ACK.
pub async fn run_session_start_bind_client(
    state_dir: &Utf8Path,
    input: impl AsyncRead + Unpin,
) -> Result<CodexThreadId, BindControlError> {
    validate_state_dir(state_dir)?;
    let bytes = timeout(CONTROL_TIMEOUT, read_frame(input))
        .await
        .map_err(|_| BindControlError::Timeout)??;
    let hook: SessionStartInput =
        serde_json::from_slice(&bytes).map_err(BindControlError::InvalidHookInput)?;
    if hook.hook_event_name != "SessionStart" {
        return Err(BindControlError::WrongHookEvent(hook.hook_event_name));
    }

    let request = BindRequest {
        version: PROTOCOL_VERSION,
        operation: BindOperation::BindCodexThread,
        thread_id: hook.session_id.clone(),
    };
    let socket_path = state_dir.join(CONTROL_SOCKET_FILE_NAME);
    let mut stream = timeout(CONTROL_TIMEOUT, connect_with_retry(&socket_path))
        .await
        .map_err(|_| BindControlError::Timeout)??;
    validate_peer(&stream)?;
    let encoded = serde_json::to_vec(&request).map_err(BindControlError::Encode)?;
    timeout(CONTROL_TIMEOUT, async {
        stream.write_all(&encoded).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| BindControlError::Timeout)?
    .map_err(|source| BindControlError::Io {
        action: "send the Codex binding request",
        path: state_dir.join(CONTROL_SOCKET_FILE_NAME),
        source,
    })?;

    let response_bytes = timeout(CONTROL_TIMEOUT, read_frame(stream))
        .await
        .map_err(|_| BindControlError::Timeout)??;
    let response: BindResponse =
        serde_json::from_slice(&response_bytes).map_err(BindControlError::InvalidResponse)?;
    match response {
        BindResponse::Bound { version, thread_id }
            if version == PROTOCOL_VERSION && thread_id == hook.session_id =>
        {
            Ok(thread_id)
        }
        BindResponse::Bound { .. } => Err(BindControlError::MismatchedAcknowledgement),
        BindResponse::Error { version, message } if version == PROTOCOL_VERSION => {
            Err(BindControlError::Daemon(message))
        }
        BindResponse::Error { .. } => Err(BindControlError::MismatchedAcknowledgement),
    }
}

fn validate_state_dir(state_dir: &Utf8Path) -> Result<(), BindControlError> {
    let metadata = std::fs::symlink_metadata(state_dir.as_std_path()).map_err(|source| {
        BindControlError::Io {
            action: "inspect the Codex state directory",
            path: state_dir.to_owned(),
            source,
        }
    })?;
    if !metadata.file_type().is_dir() {
        return Err(BindControlError::UnsafeStateDirectory {
            path: state_dir.to_owned(),
            reason: "path is not a non-symlink directory",
        });
    }
    let expected_uid = rustix::process::geteuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(BindControlError::UnsafeStateDirectory {
            path: state_dir.to_owned(),
            reason: "directory is not owned by the effective user",
        });
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(BindControlError::UnsafeStateDirectory {
            path: state_dir.to_owned(),
            reason: "directory is group- or world-writable",
        });
    }
    Ok(())
}

fn validate_peer(stream: &UnixStream) -> Result<(), BindControlError> {
    let expected_uid = rustix::process::geteuid().as_raw();
    let actual_uid = stream
        .peer_cred()
        .map_err(BindControlError::PeerCredentials)?
        .uid();
    validate_peer_uid(expected_uid, actual_uid)
}

fn validate_peer_uid(expected_uid: u32, actual_uid: u32) -> Result<(), BindControlError> {
    if actual_uid != expected_uid {
        return Err(BindControlError::PeerIdentity {
            expected_uid,
            actual_uid,
        });
    }
    Ok(())
}

async fn handle_connection(
    stream: &mut UnixStream,
    binder: &CodexThreadBinder,
    socket_path: &Utf8Path,
) -> Result<(), BindControlError> {
    let request_bytes = read_frame(&mut *stream).await?;
    let response = match serde_json::from_slice::<BindRequest>(&request_bytes) {
        Ok(request) if request.version == PROTOCOL_VERSION => match request.operation {
            BindOperation::BindCodexThread => match binder.bind(request.thread_id.clone()).await {
                Ok(()) => BindResponse::Bound {
                    version: PROTOCOL_VERSION,
                    thread_id: request.thread_id,
                },
                Err(error) => BindResponse::Error {
                    version: PROTOCOL_VERSION,
                    message: error.to_string(),
                },
            },
        },
        Ok(_) => BindResponse::Error {
            version: PROTOCOL_VERSION,
            message: "unsupported bind control protocol version".to_owned(),
        },
        Err(error) => BindResponse::Error {
            version: PROTOCOL_VERSION,
            message: format!("invalid bind control request: {error}"),
        },
    };
    let encoded = serde_json::to_vec(&response).map_err(BindControlError::Encode)?;
    stream
        .write_all(&encoded)
        .await
        .map_err(|source| BindControlError::Io {
            action: "write a Codex binding response",
            path: socket_path.to_owned(),
            source,
        })?;
    stream
        .shutdown()
        .await
        .map_err(|source| BindControlError::Io {
            action: "finish a Codex binding response",
            path: socket_path.to_owned(),
            source,
        })
}

fn log_connection_completion(completed: Option<ConnectionCompletion>) {
    match completed {
        Some(Ok(Ok(Ok(())))) => {}
        Some(Ok(Ok(Err(error)))) => {
            tracing::warn!(%error, "Codex bind control request failed");
        }
        Some(Ok(Err(error))) => {
            tracing::warn!(%error, "Codex bind control request timed out");
        }
        Some(Err(error)) => {
            tracing::error!(%error, "Codex bind control connection task failed");
        }
        None => {}
    }
}

fn is_retryable_accept_error(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::ConnectionAborted)
        || matches!(
            error.raw_os_error(),
            Some(libc::EMFILE) | Some(libc::ENFILE)
        )
}

async fn connect_with_retry(socket_path: &Utf8Path) -> Result<UnixStream, BindControlError> {
    let mut retry_delay = INITIAL_CONNECT_RETRY_DELAY;
    loop {
        match UnixStream::connect(socket_path.as_std_path()).await {
            Ok(stream) => return Ok(stream),
            Err(source)
                if matches!(
                    source.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                sleep(retry_delay).await;
                retry_delay = retry_delay.saturating_mul(2).min(MAX_CONNECT_RETRY_DELAY);
            }
            Err(source) => {
                return Err(BindControlError::Io {
                    action: "connect to the Codex bind control socket",
                    path: socket_path.to_owned(),
                    source,
                });
            }
        }
    }
}

async fn read_frame(input: impl AsyncRead + Unpin) -> Result<Vec<u8>, BindControlError> {
    let mut bytes = Vec::new();
    input
        .take(MAX_FRAME_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(BindControlError::Read)?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_FRAME_BYTES {
        return Err(BindControlError::InvalidFrameSize(bytes.len()));
    }
    Ok(bytes)
}

async fn bind_listener(socket_path: &Utf8Path) -> Result<UnixListener, BindControlError> {
    match bind_private_listener(socket_path) {
        Ok(listener) => Ok(listener),
        Err(source) if source.kind() == io::ErrorKind::AddrInUse => {
            match UnixStream::connect(socket_path.as_std_path()).await {
                Ok(_) => return Err(BindControlError::ActiveSocket(socket_path.to_owned())),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) => {}
                Err(source) => {
                    return Err(BindControlError::Io {
                        action: "check an existing Codex bind control socket",
                        path: socket_path.to_owned(),
                        source,
                    });
                }
            }
            let metadata =
                std::fs::symlink_metadata(socket_path.as_std_path()).map_err(|source| {
                    BindControlError::Io {
                        action: "inspect a stale Codex bind control socket",
                        path: socket_path.to_owned(),
                        source,
                    }
                })?;
            if !metadata.file_type().is_socket()
                || metadata.uid() != rustix::process::geteuid().as_raw()
            {
                return Err(BindControlError::UnsafeStalePath(socket_path.to_owned()));
            }
            std::fs::remove_file(socket_path.as_std_path()).map_err(|source| {
                BindControlError::Io {
                    action: "remove a stale Codex bind control socket",
                    path: socket_path.to_owned(),
                    source,
                }
            })?;
            bind_private_listener(socket_path).map_err(|source| BindControlError::Io {
                action: "bind the Codex control socket after stale cleanup",
                path: socket_path.to_owned(),
                source,
            })
        }
        Err(source) => Err(BindControlError::Io {
            action: "bind the Codex control socket",
            path: socket_path.to_owned(),
            source,
        }),
    }
}

fn bind_private_listener(socket_path: &Utf8Path) -> io::Result<UnixListener> {
    let parent = socket_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent"))?;
    let staging = tempfile::Builder::new()
        .prefix(STAGING_DIRECTORY_PREFIX)
        .rand_bytes(STAGING_DIRECTORY_RANDOM_BYTES)
        .tempdir_in(parent.as_std_path())?;
    std::fs::set_permissions(staging.path(), std::fs::Permissions::from_mode(0o700))?;
    let staged_socket = staging.path().join(STAGED_SOCKET_FILE_NAME);
    let listener = UnixListener::bind(&staged_socket)?;
    std::fs::set_permissions(&staged_socket, std::fs::Permissions::from_mode(0o600))?;

    // Publish the already-protected socket inode with an atomic, no-replace
    // hard link. The public pathname therefore never exists with ambient
    // permissions, and a concurrent owner cannot be overwritten.
    std::fs::hard_link(&staged_socket, socket_path.as_std_path()).map_err(|source| {
        if source.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(io::ErrorKind::AddrInUse, source)
        } else {
            source
        }
    })?;
    Ok(listener)
}

struct SocketGuard {
    path: Utf8PathBuf,
    device: u64,
    inode: u64,
}

impl SocketGuard {
    fn new(path: Utf8PathBuf) -> Result<Self, BindControlError> {
        let metadata = std::fs::symlink_metadata(path.as_std_path()).map_err(|source| {
            BindControlError::Io {
                action: "inspect the Codex bind control socket",
                path: path.clone(),
                source,
            }
        })?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(self.path.as_std_path()) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = std::fs::remove_file(self.path.as_std_path());
        }
    }
}

/// Failure in the local Codex thread-binding control path.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BindControlError {
    #[error("failed to read a binding protocol frame")]
    Read(#[source] io::Error),
    #[error("binding protocol frame must contain 1 to {MAX_FRAME_BYTES} bytes, got {0}")]
    InvalidFrameSize(usize),
    #[error("invalid Codex SessionStart hook input")]
    InvalidHookInput(#[source] serde_json::Error),
    #[error("expected a SessionStart hook event, got `{0}`")]
    WrongHookEvent(String),
    #[error("failed to encode a binding protocol message")]
    Encode(#[source] serde_json::Error),
    #[error("invalid response from the Codex bind control daemon")]
    InvalidResponse(#[source] serde_json::Error),
    #[error("the Codex bind control daemon did not return the expected acknowledgement")]
    MismatchedAcknowledgement,
    #[error("Codex bind control daemon rejected the binding: {0}")]
    Daemon(String),
    #[error("Codex bind control operation timed out")]
    Timeout,
    #[error("an active Codex bind control listener already owns `{0}`")]
    ActiveSocket(Utf8PathBuf),
    #[error("refusing to replace non-socket path `{0}`")]
    UnsafeStalePath(Utf8PathBuf),
    #[error("unsafe Codex state directory `{path}`: {reason}")]
    UnsafeStateDirectory {
        path: Utf8PathBuf,
        reason: &'static str,
    },
    #[error("failed to read Unix peer credentials")]
    PeerCredentials(#[source] io::Error),
    #[error(
        "refusing Codex bind control peer owned by uid {actual_uid}; expected uid {expected_uid}"
    )]
    PeerIdentity { expected_uid: u32, actual_uid: u32 },
    #[error("failed to {action} at `{path}`")]
    Io {
        action: &'static str,
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        sync::Arc,
    };
    use tempfile::TempDir;
    use tokio::sync::Barrier;

    fn temp_path(dir: &TempDir) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("UTF-8 temp path")
    }

    fn hook_input(source: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "session_id": "thread-a",
            "hook_event_name": "SessionStart",
            "source": source
        }))
        .expect("hook JSON")
    }

    #[tokio::test]
    async fn bind_rolls_back_durable_state_when_live_worker_is_gone() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        drop(binding_rx);
        let binder = CodexThreadBinder::new(queue, binding_tx);

        let error = binder
            .bind(CodexThreadId::parse("thread-a").expect("thread id"))
            .await
            .expect_err("binding must fail");

        assert!(matches!(
            error,
            CodexThreadBindingError::LiveWorkerUnavailable
        ));
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("codex-inbox.json")).expect("inbox"))
                .expect("JSON inbox");
        assert_eq!(persisted["live_thread_id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn clear_succeeds_after_live_worker_receiver_is_dropped() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue, binding_tx);
        binder
            .bind(CodexThreadId::parse("thread-a").expect("thread id"))
            .await
            .expect("initial bind");
        drop(binding_rx);

        binder
            .clear()
            .await
            .expect("durable clear is sufficient after worker exit");

        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("codex-inbox.json")).expect("inbox"))
                .expect("JSON inbox");
        assert_eq!(persisted["live_thread_id"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn initial_clear_removes_stale_durable_binding_when_live_state_is_none() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        queue
            .bind_live_thread(Some(
                CodexThreadId::parse("stale-thread").expect("thread id"),
            ))
            .await
            .expect("stale durable bind");
        let (binding_tx, _binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue.clone(), binding_tx);

        binder.clear().await.expect("initial clear");

        assert!(queue.live_thread_id().await.is_none());
    }

    #[tokio::test]
    async fn identical_bind_does_not_notify_or_rewrite_durable_state() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, mut binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue.clone(), binding_tx);
        let thread_id = CodexThreadId::parse("thread-a").expect("thread id");
        binder.bind(thread_id.clone()).await.expect("initial bind");
        binding_rx.borrow_and_update();
        let inbox_path = path.join("codex-inbox.json");
        let inode = std::fs::metadata(&inbox_path)
            .expect("inbox metadata")
            .ino();

        binder.bind(thread_id).await.expect("identical bind");

        assert!(!binding_rx.has_changed().expect("binding sender"));
        assert_eq!(
            std::fs::metadata(&inbox_path)
                .expect("inbox metadata")
                .ino(),
            inode
        );
        assert_eq!(
            queue
                .status()
                .await
                .live_thread_id
                .as_ref()
                .map(CodexThreadId::as_str),
            Some("thread-a")
        );
    }

    #[tokio::test]
    async fn session_start_client_accepts_supported_sources_and_exact_ack() {
        for source in [
            "startup",
            "resume",
            "clear",
            "compact",
            "fork",
            "future-source",
        ] {
            let dir = TempDir::new().expect("temp dir");
            let path = temp_path(&dir);
            let queue = CodexEventQueue::load(&path).expect("queue");
            let (binding_tx, binding_rx) = watch::channel(None);
            let binder = CodexThreadBinder::new(queue, binding_tx);
            let cancel = CancellationToken::new();
            let listener = CodexBindControlListener::bind(&path, binder)
                .await
                .expect("listener");
            let task = tokio::spawn(listener.run(cancel.clone()));

            let bound = run_session_start_bind_client(&path, hook_input(source).as_slice())
                .await
                .expect("binding succeeds");

            assert_eq!(bound.as_str(), "thread-a");
            assert_eq!(
                binding_rx.borrow().as_ref().map(CodexThreadId::as_str),
                Some("thread-a")
            );
            cancel.cancel();
            task.await.expect("listener task").expect("listener");
        }
    }

    #[tokio::test]
    async fn session_start_client_rejects_other_hook_events() {
        let dir = TempDir::new().expect("temp dir");
        let input = serde_json::to_vec(&json!({
            "session_id": "thread-a",
            "hook_event_name": "Stop",
            "source": "startup"
        }))
        .expect("hook JSON");

        let error = run_session_start_bind_client(&temp_path(&dir), input.as_slice())
            .await
            .expect_err("wrong hook must fail");

        assert!(matches!(error, BindControlError::WrongHookEvent(event) if event == "Stop"));
    }

    #[tokio::test(start_paused = true)]
    async fn session_start_client_times_out_when_hook_input_never_reaches_eof() {
        let dir = TempDir::new().expect("temp dir");
        let (mut writer, reader) = tokio::io::duplex(MAX_FRAME_BYTES as usize);
        writer
            .write_all(&hook_input("startup"))
            .await
            .expect("hook input");
        let path = temp_path(&dir);
        let client =
            tokio::spawn(async move { run_session_start_bind_client(&path, reader).await });
        tokio::task::yield_now().await;

        tokio::time::advance(CONTROL_TIMEOUT).await;
        let error = client
            .await
            .expect("client task")
            .expect_err("open stdin must time out");

        assert!(matches!(error, BindControlError::Timeout));
        drop(writer);
    }

    #[tokio::test]
    async fn session_start_client_rejects_malformed_nonempty_json() {
        let dir = TempDir::new().expect("temp dir");

        let error = run_session_start_bind_client(&temp_path(&dir), b"{".as_slice())
            .await
            .expect_err("malformed JSON must fail");

        assert!(matches!(error, BindControlError::InvalidHookInput(_)));
    }

    #[tokio::test]
    async fn listener_refuses_to_replace_non_socket_path() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        std::fs::write(&socket_path, b"keep me").expect("sentinel");

        let error = bind_listener(&socket_path)
            .await
            .expect_err("regular file must be preserved");

        assert!(matches!(error, BindControlError::UnsafeStalePath(found) if found == socket_path));
        assert_eq!(std::fs::read(&socket_path).expect("sentinel"), b"keep me");
    }

    #[tokio::test]
    async fn client_times_out_when_matching_daemon_never_appears() {
        let dir = TempDir::new().expect("temp dir");

        let error =
            run_session_start_bind_client(&temp_path(&dir), hook_input("startup").as_slice())
                .await
                .expect_err("missing matching daemon must fail");

        assert!(matches!(error, BindControlError::Timeout));
    }

    #[tokio::test]
    async fn listener_rejects_group_writable_state_directory() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o770))
            .expect("unsafe test mode");
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, _binding_rx) = watch::channel(None);

        let error =
            match CodexBindControlListener::bind(&path, CodexThreadBinder::new(queue, binding_tx))
                .await
            {
                Ok(_) => panic!("unsafe state directory must fail"),
                Err(error) => error,
            };

        assert!(matches!(
            error,
            BindControlError::UnsafeStateDirectory {
                reason: "directory is group- or world-writable",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn client_rejects_symlink_state_directory() {
        let target = TempDir::new().expect("target dir");
        let links = TempDir::new().expect("links dir");
        let link = temp_path(&links).join("state-link");
        symlink(target.path(), &link).expect("state symlink");

        let error = run_session_start_bind_client(&link, b"{}".as_slice())
            .await
            .expect_err("symlink state directory must fail");

        assert!(matches!(
            error,
            BindControlError::UnsafeStateDirectory {
                reason: "path is not a non-symlink directory",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn listener_refuses_active_socket() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let _active = UnixListener::bind(&socket_path).expect("active listener");

        let error = bind_listener(&socket_path)
            .await
            .expect_err("active socket must not be replaced");

        assert!(matches!(error, BindControlError::ActiveSocket(found) if found == socket_path));
    }

    #[tokio::test]
    async fn listener_replaces_stale_socket_and_protects_new_socket() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        drop(UnixListener::bind(&socket_path).expect("stale listener"));
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, _binding_rx) = watch::channel(None);

        let listener =
            CodexBindControlListener::bind(&path, CodexThreadBinder::new(queue, binding_tx))
                .await
                .expect("replace stale socket");

        let mode = std::fs::symlink_metadata(&socket_path)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        UnixStream::connect(&socket_path)
            .await
            .expect("published protected socket is connectable");
        drop(listener);
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn private_staging_preserves_the_longest_supported_public_socket_path() {
        let dir = TempDir::new().expect("temp dir");
        let base = temp_path(&dir);
        let public_path_limit = std::mem::size_of::<libc::sockaddr_un>()
            - std::mem::offset_of!(libc::sockaddr_un, sun_path)
            - 1;
        let fixed_length = base.as_str().len() + 2 + CONTROL_SOCKET_FILE_NAME.len();
        let component_length = public_path_limit
            .checked_sub(fixed_length)
            .expect("temp root leaves room for a boundary path");
        assert!(component_length > 0);
        let state_dir = base.join("x".repeat(component_length));
        std::fs::create_dir(&state_dir).expect("boundary state directory");
        let socket_path = state_dir.join(CONTROL_SOCKET_FILE_NAME);
        assert_eq!(socket_path.as_str().len(), public_path_limit);

        let listener = bind_private_listener(&socket_path)
            .expect("the longest supported public socket path must bind");
        UnixStream::connect(&socket_path)
            .await
            .expect("boundary socket is connectable after publication");

        drop(listener);
    }

    #[tokio::test]
    async fn socket_guard_does_not_remove_replacement_inode() {
        let dir = TempDir::new().expect("temp dir");
        let socket_path = temp_path(&dir).join(CONTROL_SOCKET_FILE_NAME);
        let original = bind_private_listener(&socket_path).expect("original listener");
        let guard = SocketGuard::new(socket_path.clone()).expect("socket guard");
        std::fs::remove_file(&socket_path).expect("remove original pathname");
        let replacement = bind_private_listener(&socket_path).expect("replacement listener");

        drop(guard);

        assert!(socket_path.exists());
        drop(original);
        drop(replacement);
    }

    #[tokio::test]
    async fn client_rejects_oversized_hook_input() {
        let dir = TempDir::new().expect("temp dir");
        let input = vec![b'x'; MAX_FRAME_BYTES as usize + 1];

        let error = run_session_start_bind_client(&temp_path(&dir), input.as_slice())
            .await
            .expect_err("oversized input must fail");

        assert!(matches!(
            error,
            BindControlError::InvalidFrameSize(size)
                if size == MAX_FRAME_BYTES as usize + 1
        ));
    }

    #[tokio::test]
    async fn client_rejects_mismatched_acknowledgement() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let listener = UnixListener::bind(&socket_path).expect("fake listener");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .expect("protect fake listener");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client");
            validate_peer(&stream).expect("same-user peer");
            let _request = read_frame(&mut stream).await.expect("request");
            let response = BindResponse::Bound {
                version: PROTOCOL_VERSION,
                thread_id: CodexThreadId::parse("wrong-thread").expect("thread id"),
            };
            stream
                .write_all(&serde_json::to_vec(&response).expect("response JSON"))
                .await
                .expect("response");
            stream.shutdown().await.expect("shutdown");
        });

        let error = run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect_err("mismatched ACK must fail");

        assert!(matches!(error, BindControlError::MismatchedAcknowledgement));
        server.await.expect("fake server");
    }

    #[tokio::test]
    async fn client_propagates_daemon_error_response() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let listener = bind_private_listener(&socket_path).expect("fake listener");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client");
            let _request = read_frame(&mut stream).await.expect("request");
            let response = BindResponse::Error {
                version: PROTOCOL_VERSION,
                message: "durable store unavailable".to_owned(),
            };
            stream
                .write_all(&serde_json::to_vec(&response).expect("response JSON"))
                .await
                .expect("response");
            stream.shutdown().await.expect("shutdown");
        });

        let error = run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect_err("daemon rejection must propagate");

        assert!(
            matches!(error, BindControlError::Daemon(message) if message == "durable store unavailable")
        );
        server.await.expect("fake server");
    }

    #[tokio::test]
    async fn client_rejects_response_protocol_version_mismatch() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let listener = bind_private_listener(&socket_path).expect("fake listener");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("client");
            let _request = read_frame(&mut stream).await.expect("request");
            let response = BindResponse::Bound {
                version: PROTOCOL_VERSION + 1,
                thread_id: CodexThreadId::parse("thread-a").expect("thread id"),
            };
            stream
                .write_all(&serde_json::to_vec(&response).expect("response JSON"))
                .await
                .expect("response");
            stream.shutdown().await.expect("shutdown");
        });

        let error = run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect_err("version mismatch must fail");

        assert!(matches!(error, BindControlError::MismatchedAcknowledgement));
        server.await.expect("fake server");
    }

    #[tokio::test]
    async fn listener_rejects_request_protocol_version_and_keeps_serving() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue, binding_tx);
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder)
            .await
            .expect("listener");
        let task = tokio::spawn(listener.run(cancel.clone()));
        let mut stream = UnixStream::connect(path.join(CONTROL_SOCKET_FILE_NAME))
            .await
            .expect("connect");
        let request = BindRequest {
            version: PROTOCOL_VERSION + 1,
            operation: BindOperation::BindCodexThread,
            thread_id: CodexThreadId::parse("thread-a").expect("thread id"),
        };
        stream
            .write_all(&serde_json::to_vec(&request).expect("request JSON"))
            .await
            .expect("request");
        stream.shutdown().await.expect("shutdown request");
        let response: BindResponse =
            serde_json::from_slice(&read_frame(stream).await.expect("response"))
                .expect("response JSON");

        assert!(matches!(
            response,
            BindResponse::Error { version, message }
                if version == PROTOCOL_VERSION
                    && message == "unsupported bind control protocol version"
        ));
        assert!(binding_rx.borrow().is_none());
        cancel.cancel();
        task.await.expect("listener task").expect("listener");
    }

    #[tokio::test]
    async fn stalled_connection_does_not_block_a_second_binding() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue, binding_tx);
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder)
            .await
            .expect("listener");
        let task = tokio::spawn(listener.run(cancel.clone()));
        let stalled = UnixStream::connect(path.join(CONTROL_SOCKET_FILE_NAME))
            .await
            .expect("stalled connection");

        tokio::time::timeout(
            Duration::from_secs(1),
            run_session_start_bind_client(&path, hook_input("startup").as_slice()),
        )
        .await
        .expect("second connection must not head-of-line block")
        .expect("binding succeeds");

        assert_eq!(
            binding_rx.borrow().as_ref().map(CodexThreadId::as_str),
            Some("thread-a")
        );
        drop(stalled);
        cancel.cancel();
        task.await.expect("listener task").expect("listener");
    }

    #[tokio::test]
    async fn client_retries_until_listener_appears() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let delayed_path = path.clone();
        let server = tokio::spawn(async move {
            sleep(Duration::from_millis(75)).await;
            let listener = bind_private_listener(&delayed_path.join(CONTROL_SOCKET_FILE_NAME))
                .expect("delayed listener");
            let (mut stream, _) = listener.accept().await.expect("client");
            let request: BindRequest =
                serde_json::from_slice(&read_frame(&mut stream).await.expect("request"))
                    .expect("request JSON");
            let response = BindResponse::Bound {
                version: PROTOCOL_VERSION,
                thread_id: request.thread_id,
            };
            stream
                .write_all(&serde_json::to_vec(&response).expect("response JSON"))
                .await
                .expect("response");
            stream.shutdown().await.expect("shutdown");
        });

        let bound = run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect("eventual binding");

        assert_eq!(bound.as_str(), "thread-a");
        server.await.expect("delayed server");
    }

    #[test]
    fn peer_uid_validation_allows_matching_uid_and_rejects_other_uid() {
        assert!(validate_peer_uid(1000, 1000).is_ok());
        assert!(matches!(
            validate_peer_uid(1000, 1001),
            Err(BindControlError::PeerIdentity {
                expected_uid: 1000,
                actual_uid: 1001
            })
        ));
    }

    #[test]
    fn accept_error_retry_policy_covers_resource_pressure_and_aborted_connections() {
        assert!(is_retryable_accept_error(&io::Error::from_raw_os_error(
            libc::EMFILE
        )));
        assert!(is_retryable_accept_error(&io::Error::from_raw_os_error(
            libc::ENFILE
        )));
        assert!(is_retryable_accept_error(&io::Error::from(
            io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_retryable_accept_error(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
    }

    #[tokio::test]
    async fn concurrent_binds_keep_durable_and_live_state_consistent() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue, binding_tx);
        let barrier = Arc::new(Barrier::new(3));
        let first = tokio::spawn({
            let binder = binder.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                binder
                    .bind(CodexThreadId::parse("thread-a").expect("thread id"))
                    .await
            }
        });
        let second = tokio::spawn({
            let binder = binder.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                binder
                    .bind(CodexThreadId::parse("thread-b").expect("thread id"))
                    .await
            }
        });
        barrier.wait().await;
        first.await.expect("first task").expect("first bind");
        second.await.expect("second task").expect("second bind");

        let live = binding_rx
            .borrow()
            .as_ref()
            .expect("live binding")
            .as_str()
            .to_owned();
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("codex-inbox.json")).expect("inbox"))
                .expect("JSON inbox");
        assert!(matches!(live.as_str(), "thread-a" | "thread-b"));
        assert_eq!(persisted["live_thread_id"], live);
    }

    #[tokio::test]
    async fn shutdown_clears_binding_removes_socket_and_releases_queue_lock() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue.clone(), binding_tx);
        binder
            .bind(CodexThreadId::parse("thread-a").expect("thread id"))
            .await
            .expect("initial bind");
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder.clone())
            .await
            .expect("listener");
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let task = tokio::spawn(listener.run(cancel.clone()));

        binder.clear().await.expect("shutdown clear");
        cancel.cancel();
        task.await
            .expect("listener task")
            .expect("listener shutdown");
        assert!(binding_rx.borrow().is_none());
        assert!(!socket_path.exists());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("codex-inbox.json")).expect("inbox"))
                .expect("JSON inbox");
        assert_eq!(persisted["live_thread_id"], serde_json::Value::Null);

        drop(binding_rx);
        drop(binder);
        drop(queue);
        CodexEventQueue::load(&path).expect("queue lock released");
    }
}
