//! Local control path for binding a Codex session to live delivery.

use crate::codex::{CodexEventQueue, CodexThreadId, LiveBinding};
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::{
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
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
const STAGING_RELATIVE_PATH_BYTES: usize = STAGING_DIRECTORY_PREFIX.len()
    + STAGING_DIRECTORY_RANDOM_BYTES
    + 1
    + STAGED_SOCKET_FILE_NAME.len();
const _: () = assert!(STAGING_RELATIVE_PATH_BYTES <= CONTROL_SOCKET_FILE_NAME.len());
type ConnectionResult = Result<Result<(), BindControlError>, tokio::time::error::Elapsed>;
type ConnectionCompletion = Result<ConnectionResult, tokio::task::JoinError>;

/// Synchronous façade over the process-local Codex thread binding.
#[derive(Clone)]
pub struct CodexThreadBinder {
    live_binding: LiveBinding,
}

impl CodexThreadBinder {
    /// Creates a binder over the queue's process-local routing state.
    pub fn new(queue: CodexEventQueue) -> Self {
        Self {
            live_binding: queue.live_binding(),
        }
    }

    /// Publishes a binding unless shutdown has fenced future updates.
    pub fn bind(&self, thread_id: CodexThreadId) -> Result<(), CodexThreadBindingError> {
        let changed = self.live_binding.bind(thread_id.clone())?;
        if changed {
            tracing::info!(
                thread_id = thread_id.as_str(),
                "Codex thread binding changed"
            );
        } else {
            tracing::debug!(
                thread_id = thread_id.as_str(),
                "Codex thread binding is already current"
            );
        }
        Ok(())
    }

    /// Clears the process-local Codex thread binding.
    pub fn clear(&self) {
        if self.live_binding.clear() {
            tracing::info!("Codex thread binding cleared");
        }
    }

    /// Fences future binding changes while preserving the final ingress tag.
    ///
    /// The notification task flushes its delivery buffer after cancellation,
    /// so the current tag must remain available until that drain completes.
    pub fn begin_shutdown(&self) {
        self.live_binding.begin_shutdown();
    }
}

/// Failure while changing the canonical Codex thread binding.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CodexThreadBindingError {
    #[error("Codex live binding is stopping")]
    Stopping,
}

#[derive(Debug, Deserialize)]
struct SessionStartInput {
    session_id: CodexThreadId,
    hook_event_name: String,
    #[serde(default, rename = "source")]
    _source: Option<serde_json::Value>,
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
            BindOperation::BindCodexThread => match binder.bind(request.thread_id.clone()) {
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
            Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOMEM) | Some(libc::ENOBUFS)
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
    validate_public_socket_path(socket_path)?;
    match bind_private_listener(socket_path) {
        Ok(listener) => Ok(listener),
        Err(BindControlError::PublishSocket { source, .. })
            if source.kind() == io::ErrorKind::AlreadyExists =>
        {
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
            bind_private_listener(socket_path)
        }
        Err(error) => Err(error),
    }
}

fn validate_public_socket_path(socket_path: &Utf8Path) -> Result<(), BindControlError> {
    let length = socket_path.as_std_path().as_os_str().as_bytes().len();
    let max = max_unix_socket_path_bytes();
    if length > max {
        return Err(BindControlError::SocketPathTooLong {
            path: socket_path.to_owned(),
            length,
            max,
        });
    }
    Ok(())
}

const fn max_unix_socket_path_bytes() -> usize {
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path) - 1
}

fn create_private_staging_directory(
    parent: &Utf8Path,
) -> Result<tempfile::TempDir, BindControlError> {
    let staging = tempfile::Builder::new()
        .prefix(STAGING_DIRECTORY_PREFIX)
        .rand_bytes(STAGING_DIRECTORY_RANDOM_BYTES)
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(parent.as_std_path())
        .map_err(|source| BindControlError::StagingDirectory {
            parent: parent.to_owned(),
            source,
        })?;
    let metadata =
        std::fs::symlink_metadata(staging.path()).map_err(|source| BindControlError::Io {
            action: "inspect the private Codex control staging directory",
            path: Utf8PathBuf::from_path_buf(staging.path().to_owned())
                .unwrap_or_else(|_| parent.to_owned()),
            source,
        })?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(BindControlError::UnsafeStagingDirectory(
            Utf8PathBuf::from_path_buf(staging.path().to_owned())
                .unwrap_or_else(|_| parent.to_owned()),
        ));
    }
    // On POSIX ACL filesystems, the group mode bits are the ACL mask. Exact
    // 0700 verification therefore also rejects inherited ACL access outside
    // the owner class.
    Ok(staging)
}

fn bind_private_listener(socket_path: &Utf8Path) -> Result<UnixListener, BindControlError> {
    validate_public_socket_path(socket_path)?;
    let parent = socket_path
        .parent()
        .ok_or_else(|| BindControlError::InvalidSocketPath(socket_path.to_owned()))?;
    let staging = create_private_staging_directory(parent)?;
    let staged_socket = staging.path().join(STAGED_SOCKET_FILE_NAME);
    let staged_path = Utf8PathBuf::from_path_buf(staged_socket.clone())
        .map_err(|_| BindControlError::InvalidSocketPath(socket_path.to_owned()))?;
    let listener =
        UnixListener::bind(&staged_socket).map_err(|source| BindControlError::StagedBind {
            path: staged_path.clone(),
            source,
        })?;
    std::fs::set_permissions(&staged_socket, std::fs::Permissions::from_mode(0o600)).map_err(
        |source| BindControlError::ProtectStagedSocket {
            path: staged_path,
            source,
        },
    )?;

    // Publish the already-protected socket inode with an atomic, no-replace
    // hard link. The public pathname therefore never exists with ambient
    // permissions, and a concurrent owner cannot be overwritten.
    std::fs::hard_link(&staged_socket, socket_path.as_std_path()).map_err(|source| {
        BindControlError::PublishSocket {
            path: socket_path.to_owned(),
            source,
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
    #[error("Codex bind control socket path `{path}` is {length} bytes; maximum is {max}")]
    SocketPathTooLong {
        path: Utf8PathBuf,
        length: usize,
        max: usize,
    },
    #[error("invalid Codex bind control socket path `{0}`")]
    InvalidSocketPath(Utf8PathBuf),
    #[error("failed to create a private Codex control staging directory under `{parent}`")]
    StagingDirectory {
        parent: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("unsafe private Codex control staging directory `{0}`")]
    UnsafeStagingDirectory(Utf8PathBuf),
    #[error("failed to bind staged Codex control socket `{path}`")]
    StagedBind {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to protect staged Codex control socket `{path}`")]
    ProtectStagedSocket {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to publish Codex control socket `{path}`")]
    PublishSocket {
        path: Utf8PathBuf,
        #[source]
        source: io::Error,
    },
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
        process::Command,
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

    #[test]
    fn bind_request_exact_wire_fixture() {
        let request = BindRequest {
            version: 1,
            operation: BindOperation::BindCodexThread,
            thread_id: CodexThreadId::parse("thread-a").expect("thread id"),
        };

        assert_eq!(
            serde_json::to_string(&request).expect("request JSON"),
            r#"{"version":1,"operation":"bind_codex_thread","thread_id":"thread-a"}"#
        );
        let decoded: BindRequest = serde_json::from_str(
            r#"{"version":1,"operation":"bind_codex_thread","thread_id":"thread-a"}"#,
        )
        .expect("literal request fixture");
        assert_eq!(decoded.version, 1);
        assert!(matches!(decoded.operation, BindOperation::BindCodexThread));
        assert_eq!(decoded.thread_id.as_str(), "thread-a");
    }

    #[test]
    fn bind_response_exact_wire_fixtures() {
        let bound = BindResponse::Bound {
            version: 1,
            thread_id: CodexThreadId::parse("thread-a").expect("thread id"),
        };
        let error = BindResponse::Error {
            version: 1,
            message: "binding stopped".to_owned(),
        };

        assert_eq!(
            serde_json::to_string(&bound).expect("bound JSON"),
            r#"{"status":"bound","version":1,"thread_id":"thread-a"}"#
        );
        assert_eq!(
            serde_json::to_string(&error).expect("error JSON"),
            r#"{"status":"error","version":1,"message":"binding stopped"}"#
        );
        assert!(matches!(
            serde_json::from_str::<BindResponse>(
                r#"{"status":"bound","version":1,"thread_id":"thread-a"}"#
            )
            .expect("literal bound fixture"),
            BindResponse::Bound { version: 1, thread_id }
                if thread_id.as_str() == "thread-a"
        ));
        assert!(matches!(
            serde_json::from_str::<BindResponse>(
                r#"{"status":"error","version":1,"message":"binding stopped"}"#
            )
            .expect("literal error fixture"),
            BindResponse::Error { version: 1, message }
                if message == "binding stopped"
        ));
    }

    #[tokio::test]
    async fn bind_and_clear_are_ephemeral_even_when_persistence_is_broken() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let mut binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue.clone());
        let thread_id = CodexThreadId::parse("thread-a").expect("thread id");
        let inbox_path = path.join("codex-inbox.json");
        queue
            .enqueue(json!({"seed": "durable inode"}))
            .await
            .expect("seed inbox");
        let before = std::fs::metadata(&inbox_path).expect("inbox metadata");
        queue.inbox.lock().await.temporary_path = path.join("missing").join("inbox.tmp");

        binder.bind(thread_id.clone()).expect("bind is in-memory");
        binding_rx.borrow_and_update();
        binder.bind(thread_id).expect("identical bind");

        assert!(!binding_rx.has_changed().expect("binding sender"));
        binder.clear();
        assert!(binding_rx.has_changed().expect("binding sender"));
        let after = std::fs::metadata(&inbox_path).expect("inbox metadata");
        assert_eq!(after.ino(), before.ino());
        assert_eq!(after.modified().unwrap(), before.modified().unwrap());
        assert_eq!(
            queue.status().await.live_thread_id,
            None,
            "status reports process-local state"
        );
    }

    #[tokio::test]
    async fn begin_shutdown_preserves_ingress_tag_and_fences_future_binds() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue.clone());
        binder
            .bind(CodexThreadId::parse("thread-a").expect("thread id"))
            .expect("initial bind");

        binder.begin_shutdown();

        assert!(binding_rx.borrow().is_none());
        assert!(matches!(
            binder.bind(CodexThreadId::parse("thread-b").expect("thread id")),
            Err(CodexThreadBindingError::Stopping)
        ));
        binder.clear();
        assert!(binding_rx.borrow().is_none());
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder.clone())
            .await
            .expect("listener");
        let task = tokio::spawn(listener.run(cancel.clone()));
        let error = run_session_start_bind_client(&path, hook_input("resume").as_slice())
            .await
            .expect_err("control bind must observe shutdown fence");
        assert!(matches!(error, BindControlError::Daemon(_)));
        cancel.cancel();
        task.await.unwrap().unwrap();
        assert!(queue.status().await.live_thread_id.is_none());
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
            let binding_rx = queue.subscribe_live_binding();
            let binder = CodexThreadBinder::new(queue);
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
    async fn session_start_client_ignores_missing_or_nonstring_source() {
        for source in [None, Some(json!({ "future": true }))] {
            let dir = TempDir::new().expect("temp dir");
            let path = temp_path(&dir);
            let queue = CodexEventQueue::load(&path).expect("queue");
            let binder = CodexThreadBinder::new(queue);
            let cancel = CancellationToken::new();
            let listener = CodexBindControlListener::bind(&path, binder)
                .await
                .expect("listener");
            let task = tokio::spawn(listener.run(cancel.clone()));
            let mut input = json!({
                "session_id": "thread-a",
                "hook_event_name": "SessionStart"
            });
            if let Some(source) = source {
                input["source"] = source;
            }

            run_session_start_bind_client(
                &path,
                serde_json::to_vec(&input).expect("hook JSON").as_slice(),
            )
            .await
            .expect("source is optional and ignored");

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
        let error = match CodexBindControlListener::bind(&path, CodexThreadBinder::new(queue)).await
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
        let listener = CodexBindControlListener::bind(&path, CodexThreadBinder::new(queue))
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
        let public_path_limit = max_unix_socket_path_bytes();
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

    #[test]
    fn staging_path_budget_never_exceeds_public_socket_name() {
        assert_eq!(STAGING_RELATIVE_PATH_BYTES, 11);
        assert!(STAGING_RELATIVE_PATH_BYTES <= CONTROL_SOCKET_FILE_NAME.len());
        assert!(max_unix_socket_path_bytes() > CONTROL_SOCKET_FILE_NAME.len());
    }

    #[test]
    fn staging_directory_is_private_at_creation_under_permissive_umask() {
        const CHILD_ENV: &str = "DIONE_STAGING_UMASK_CHILD";
        const TEST_NAME: &str = "codex::bind_control::tests::staging_directory_is_private_at_creation_under_permissive_umask";
        if std::env::var_os(CHILD_ENV).is_some() {
            // SAFETY: this runs in a dedicated child process, so the
            // process-global umask cannot affect another test or Dione task.
            unsafe {
                libc::umask(0);
            }
            let dir = TempDir::new().expect("temp dir");
            let staging = create_private_staging_directory(&temp_path(&dir))
                .expect("private staging directory");
            let metadata = std::fs::symlink_metadata(staging.path()).expect("staging metadata");
            assert_eq!(metadata.mode() & 0o777, 0o700);
            return;
        }

        let status = Command::new(std::env::current_exe().expect("test executable"))
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ENV, "1")
            .status()
            .expect("spawn isolated umask test");

        assert!(status.success());
    }

    #[tokio::test]
    async fn overlong_public_socket_path_is_rejected_before_staging() {
        let dir = TempDir::new().expect("temp dir");
        let base = temp_path(&dir);
        let name_length = max_unix_socket_path_bytes() + 1 - base.as_str().len() - 1;
        let socket_path = base.join("x".repeat(name_length));

        let error =
            bind_private_listener(&socket_path).expect_err("overlong public socket path must fail");

        assert!(matches!(
            error,
            BindControlError::SocketPathTooLong {
                path,
                length,
                max
            } if path == socket_path
                && length == max_unix_socket_path_bytes() + 1
                && max == max_unix_socket_path_bytes()
        ));
        assert_eq!(
            std::fs::read_dir(&base).expect("state directory").count(),
            0,
            "validation must run before creating a staging directory"
        );
    }

    #[tokio::test]
    async fn public_publish_collision_has_distinct_error() {
        let dir = TempDir::new().expect("temp dir");
        let socket_path = temp_path(&dir).join(CONTROL_SOCKET_FILE_NAME);
        let first = bind_private_listener(&socket_path).expect("first listener");

        let error = bind_private_listener(&socket_path).expect_err("publish collision");

        assert!(matches!(
            error,
            BindControlError::PublishSocket { path, source }
                if path == socket_path && source.kind() == io::ErrorKind::AlreadyExists
        ));
        drop(first);
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
            let request = read_frame(&mut stream).await.expect("request");
            assert_eq!(
                request,
                br#"{"version":1,"operation":"bind_codex_thread","thread_id":"thread-a"}"#
            );
            stream
                .write_all(
                    br#"{"status":"error","version":1,"message":"process-local binding unavailable"}"#,
                )
                .await
                .expect("response");
            stream.shutdown().await.expect("shutdown");
        });

        let error = run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect_err("daemon rejection must propagate");

        assert!(
            matches!(error, BindControlError::Daemon(message) if message == "process-local binding unavailable")
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
        let binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue.clone());
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
        let binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue);
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
    async fn shutdown_drain_times_out_a_held_open_connection() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let binder = CodexThreadBinder::new(queue);
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder)
            .await
            .expect("listener");
        let task = tokio::spawn(listener.run(cancel.clone()));
        let stalled = UnixStream::connect(path.join(CONTROL_SOCKET_FILE_NAME))
            .await
            .expect("stalled connection");

        // A successful second request proves the listener accepted and
        // spawned the earlier FIFO connection before shutdown begins.
        run_session_start_bind_client(&path, hook_input("startup").as_slice())
            .await
            .expect("second connection binds");

        cancel.cancel();
        let completed_with_bound = timeout(CONTROL_TIMEOUT + Duration::from_secs(1), task).await;
        drop(stalled);
        completed_with_bound
            .expect("the per-connection timeout must bound listener shutdown")
            .expect("listener task")
            .expect("listener");
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
        assert!(is_retryable_accept_error(&io::Error::from_raw_os_error(
            libc::ENOMEM
        )));
        assert!(is_retryable_accept_error(&io::Error::from_raw_os_error(
            libc::ENOBUFS
        )));
        assert!(is_retryable_accept_error(&io::Error::from(
            io::ErrorKind::ConnectionAborted
        )));
        assert!(!is_retryable_accept_error(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
    }

    #[tokio::test]
    async fn concurrent_binds_leave_one_complete_live_state() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue.clone());
        let barrier = Arc::new(Barrier::new(3));
        let first = tokio::spawn({
            let binder = binder.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                binder.bind(CodexThreadId::parse("thread-a").expect("thread id"))
            }
        });
        let second = tokio::spawn({
            let binder = binder.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                binder.bind(CodexThreadId::parse("thread-b").expect("thread id"))
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
        assert!(matches!(live.as_str(), "thread-a" | "thread-b"));
        assert_eq!(
            queue.status().await.live_thread_id.unwrap().as_str(),
            live.as_str()
        );
    }

    #[tokio::test]
    async fn shutdown_clears_binding_removes_socket_and_releases_queue_lock() {
        let dir = TempDir::new().expect("temp dir");
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(&path).expect("queue");
        let binding_rx = queue.subscribe_live_binding();
        let binder = CodexThreadBinder::new(queue.clone());
        binder
            .bind(CodexThreadId::parse("thread-a").expect("thread id"))
            .expect("initial bind");
        let cancel = CancellationToken::new();
        let listener = CodexBindControlListener::bind(&path, binder.clone())
            .await
            .expect("listener");
        let socket_path = path.join(CONTROL_SOCKET_FILE_NAME);
        let task = tokio::spawn(listener.run(cancel.clone()));

        binder.begin_shutdown();
        cancel.cancel();
        task.await
            .expect("listener task")
            .expect("listener shutdown");
        assert!(binding_rx.borrow().is_none());
        assert!(!socket_path.exists());
        drop(binding_rx);
        drop(binder);
        drop(queue);
        let mut reloaded = None;
        for _ in 0..100 {
            match CodexEventQueue::load(&path) {
                Ok(queue) => {
                    reloaded = Some(queue);
                    break;
                }
                Err(crate::codex::CodexQueueError::InboxLocked { .. }) => {
                    sleep(Duration::from_millis(1)).await;
                }
                Err(error) => panic!("unexpected queue reload failure: {error}"),
            }
        }
        assert!(
            reloaded.is_some(),
            "queue lock must be released within the bounded teardown grace period"
        );
    }
}
