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
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const CONTROL_SOCKET_FILE_NAME: &str = "codex-control.sock";
const PROTOCOL_VERSION: u8 = 1;
const MAX_FRAME_BYTES: u64 = 4 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);

/// Serializes durable and live Codex thread binding updates.
#[derive(Clone)]
pub struct CodexThreadBinder {
    queue: CodexEventQueue,
    live_binding: watch::Sender<Option<CodexThreadId>>,
    transaction: std::sync::Arc<Mutex<()>>,
}

impl CodexThreadBinder {
    /// Creates a binder over one durable queue and live delivery worker.
    pub fn new(queue: CodexEventQueue, live_binding: watch::Sender<Option<CodexThreadId>>) -> Self {
        Self {
            queue,
            live_binding,
            transaction: std::sync::Arc::new(Mutex::new(())),
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
        let _guard = self.transaction.lock().await;
        let clearing = thread_id.is_none();
        self.queue
            .bind_live_thread(thread_id.clone())
            .await
            .map_err(CodexThreadBindingError::Persist)?;

        if self.live_binding.send(thread_id).is_err() {
            if clearing {
                return Ok(());
            }
            return match self.queue.bind_live_thread(None).await {
                Ok(()) => Err(CodexThreadBindingError::LiveWorkerUnavailable),
                Err(source) => Err(CodexThreadBindingError::Rollback {
                    source: Box::new(source),
                }),
            };
        }
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: CodexThreadId,
    hook_event_name: String,
    source: SessionStartSource,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
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
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(()),
                accepted = listener.accept() => {
                    let (mut stream, _) = accepted.map_err(|source| BindControlError::Io {
                        action: "accept a control connection",
                        path: socket_path.clone(),
                        source,
                    })?;
                    validate_peer(&stream)?;
                    let result = timeout(CONTROL_TIMEOUT, handle_connection(&mut stream, &binder)).await;
                    if let Err(error) = result {
                        tracing::warn!(%error, "Codex bind control request timed out");
                    } else if let Ok(Err(error)) = result {
                        tracing::warn!(%error, "Codex bind control request failed");
                    }
                }
            }
        }
    }
}

/// Reads a Codex `SessionStart` hook payload and requires an exact daemon ACK.
pub async fn run_session_start_bind_client(
    state_dir: &Utf8Path,
    input: impl AsyncRead + Unpin,
) -> Result<CodexThreadId, BindControlError> {
    validate_state_dir(state_dir)?;
    let bytes = read_frame(input).await?;
    let hook: SessionStartInput =
        serde_json::from_slice(&bytes).map_err(BindControlError::InvalidHookInput)?;
    if hook.hook_event_name != "SessionStart" {
        return Err(BindControlError::WrongHookEvent(hook.hook_event_name));
    }
    let _source = hook.source;

    let request = BindRequest {
        version: PROTOCOL_VERSION,
        operation: BindOperation::BindCodexThread,
        thread_id: hook.session_id.clone(),
    };
    let socket_path = state_dir.join(CONTROL_SOCKET_FILE_NAME);
    let mut stream = timeout(
        CONTROL_TIMEOUT,
        UnixStream::connect(socket_path.as_std_path()),
    )
    .await
    .map_err(|_| BindControlError::Timeout)?
    .map_err(|source| BindControlError::Io {
        action: "connect to the Codex bind control socket",
        path: socket_path,
        source,
    })?;
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
            path: Utf8PathBuf::from(CONTROL_SOCKET_FILE_NAME),
            source,
        })?;
    stream
        .shutdown()
        .await
        .map_err(|source| BindControlError::Io {
            action: "finish a Codex binding response",
            path: Utf8PathBuf::from(CONTROL_SOCKET_FILE_NAME),
            source,
        })
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
    match UnixListener::bind(socket_path.as_std_path()) {
        Ok(listener) => secure_listener(listener, socket_path),
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
            let listener = UnixListener::bind(socket_path.as_std_path()).map_err(|source| {
                BindControlError::Io {
                    action: "bind the Codex control socket after stale cleanup",
                    path: socket_path.to_owned(),
                    source,
                }
            })?;
            secure_listener(listener, socket_path)
        }
        Err(source) => Err(BindControlError::Io {
            action: "bind the Codex control socket",
            path: socket_path.to_owned(),
            source,
        }),
    }
}

fn secure_listener(
    listener: UnixListener,
    socket_path: &Utf8Path,
) -> Result<UnixListener, BindControlError> {
    if let Err(source) = std::fs::set_permissions(
        socket_path.as_std_path(),
        std::fs::Permissions::from_mode(0o600),
    ) {
        let _ = std::fs::remove_file(socket_path.as_std_path());
        return Err(BindControlError::Io {
            action: "protect the Codex bind control socket",
            path: socket_path.to_owned(),
            source,
        });
    }
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
    use std::os::unix::fs::{PermissionsExt, symlink};
    use tempfile::TempDir;

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
    async fn session_start_client_accepts_supported_sources_and_exact_ack() {
        for source in ["startup", "resume", "clear", "compact"] {
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
    async fn client_rejects_state_directory_without_matching_daemon() {
        let dir = TempDir::new().expect("temp dir");

        let error =
            run_session_start_bind_client(&temp_path(&dir), hook_input("startup").as_slice())
                .await
                .expect_err("missing matching daemon must fail");

        assert!(matches!(
            error,
            BindControlError::Io {
                action: "connect to the Codex bind control socket",
                ..
            }
        ));
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
        drop(listener);
        assert!(!socket_path.exists());
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
    async fn concurrent_binds_keep_durable_and_live_state_consistent() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let (binding_tx, binding_rx) = watch::channel(None);
        let binder = CodexThreadBinder::new(queue, binding_tx);
        let first = tokio::spawn({
            let binder = binder.clone();
            async move {
                binder
                    .bind(CodexThreadId::parse("thread-a").expect("thread id"))
                    .await
            }
        });
        let second = tokio::spawn({
            let binder = binder.clone();
            async move {
                binder
                    .bind(CodexThreadId::parse("thread-b").expect("thread id"))
                    .await
            }
        });
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
