//! Codex queue, binding, and delivery task ownership.
use super::{
    HarnessConnection,
    session::{HarnessSessionError, HarnessSessionErrorKind},
};
use crate::{
    attention::delivery::AttentionDeliveryGuard,
    codex::{CodexDeliveryConfig, CodexEventQueue, CodexThreadId, run_delivery_worker},
    config::DeliveryConfig,
    durable::EndpointId,
};
use camino::{Utf8Path, Utf8PathBuf};
use std::{env, sync::Arc};
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;

/// Optional CLI overrides resolved only when live delivery starts.
pub struct CodexSessionOptions {
    legacy_scope: crate::codex::LegacyDiscordScope,
    socket_path: Option<Utf8PathBuf>,
    initial_thread: Option<CodexThreadId>,
}

impl CodexSessionOptions {
    /// Retains overrides without reading environment or opening a socket.
    pub fn new(
        socket_path: Option<Utf8PathBuf>,
        initial_thread: Option<CodexThreadId>,
        legacy_scope: crate::codex::LegacyDiscordScope,
    ) -> Self {
        Self {
            legacy_scope,
            socket_path,
            initial_thread,
        }
    }
}

pub(super) struct CodexSession {
    queue: CodexEventQueue,
    binding_tx: watch::Sender<Option<EndpointId>>,
    binding_rx: watch::Receiver<Option<EndpointId>>,
    options: CodexSessionOptions,
}

pub(super) struct RunningCodexSession {
    queue: CodexEventQueue,
    cancellation: CancellationToken,
    worker: JoinHandle<()>,
}

impl CodexSession {
    pub(super) async fn prepare(
        state_dir: &Utf8Path,
        options: CodexSessionOptions,
    ) -> Result<Self, HarnessSessionError> {
        let queue = CodexEventQueue::open(state_dir, options.legacy_scope.clone())
            .await
            .map_err(|source| HarnessSessionError {
                kind: HarnessSessionErrorKind::QueueOpen(source),
            })?;
        let (binding_tx, binding_rx) = watch::channel(None);
        Ok(Self {
            queue,
            binding_tx,
            binding_rx,
            options,
        })
    }

    pub(super) fn connection(&self) -> HarnessConnection {
        HarnessConnection::codex(self.queue.clone(), self.binding_tx.clone())
    }

    pub(super) fn codex_admission(&self) -> CodexEventQueue {
        self.queue.clone()
    }

    pub(super) async fn start(
        self,
        delivery: &DeliveryConfig,
        attention: Arc<dyn AttentionDeliveryGuard>,
        process_shutdown: CancellationToken,
    ) -> Result<RunningCodexSession, HarnessSessionError> {
        self.start_with_thread_environment(delivery, attention, process_shutdown, || {
            env::var("CODEX_THREAD_ID").ok()
        })
        .await
    }

    async fn start_with_thread_environment(
        self,
        delivery: &DeliveryConfig,
        attention: Arc<dyn AttentionDeliveryGuard>,
        process_shutdown: CancellationToken,
        environment_thread: impl FnOnce() -> Option<String>,
    ) -> Result<RunningCodexSession, HarnessSessionError> {
        let mut config = CodexDeliveryConfig::resolve(
            self.options.socket_path,
            self.options.legacy_scope.clone(),
        )
        .map_err(|source| HarnessSessionError {
            kind: HarnessSessionErrorKind::Configure(source),
        })?;
        config.preamble_mode = delivery.preamble_mode;
        config.preamble_template = delivery.preamble_template.clone();
        config.attention_guard = Some(attention);
        let initial_thread = match self.options.initial_thread {
            Some(thread) => Some(thread),
            None => environment_thread()
                .map(|value| value.parse())
                .transpose()
                .map_err(|source| HarnessSessionError {
                    kind: HarnessSessionErrorKind::EnvironmentThread(source),
                })?,
        };
        self.queue
            .binding_controller()
            .bind_live_thread_and_publish(initial_thread, &self.binding_tx)
            .await
            .map_err(|source| HarnessSessionError {
                kind: HarnessSessionErrorKind::InitialBinding(source),
            })?;
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let queue = self.queue.clone();
        let worker = tokio::spawn(async move {
            if let Err(error) =
                run_delivery_worker(self.queue, config, self.binding_rx, worker_cancel).await
            {
                tracing::error!(error = %error, "Codex live delivery worker exited");
                process_shutdown.cancel();
            }
        });
        Ok(RunningCodexSession {
            queue,
            cancellation,
            worker,
        })
    }
}

impl RunningCodexSession {
    pub(super) async fn shutdown(self) -> Result<(), HarnessSessionError> {
        self.cancellation.cancel();
        let result = self.worker.await.map_err(|source| HarnessSessionError {
            kind: HarnessSessionErrorKind::Join(source),
        });
        self.queue.drain_owned().await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::{
        super::session::tests::{Guard, peer, receive, wait_for_worker},
        *,
    };
    use serde_json::json;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::{net::UnixListener, sync::Notify, time::timeout};

    fn options(path: Utf8PathBuf) -> CodexSessionOptions {
        CodexSessionOptions::new(
            Some(path),
            Some(CodexThreadId::parse("registration-thread").unwrap()),
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
    }

    async fn registration_uncertainty(cancel_while_pending: bool) {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let path = root.join("app.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let session = CodexSession::prepare(&root, options(path)).await.unwrap();
        let queue = session.codex_admission();
        let prepared = queue.test_snapshot().await;
        assert!(
            prepared.consumers.is_empty(),
            "prepare must not register a resident"
        );
        assert!(prepared.primary_consumer.is_none());
        let connection = session.connection();
        let shutdown = CancellationToken::new();
        let process_shutdown = shutdown.clone();
        let attempts_before = queue.test_directory_sync_attempts().await;
        let (reached, release) = queue.pause_binding_publish_for_test();
        let starting = tokio::spawn(async move {
            session
                .start_with_thread_environment(
                    &DeliveryConfig::default(),
                    Arc::new(Guard),
                    process_shutdown,
                    || None,
                )
                .await
        });
        timeout(Duration::from_secs(5), reached)
            .await
            .unwrap()
            .unwrap();
        let fault = queue.test_set_persist_failure(Some(crate::codex::PersistFailure::AfterRename));
        tokio::pin!(fault);
        assert!(futures_util::poll!(fault.as_mut()).is_pending());
        release.send(()).unwrap();
        fault.await;
        let running = timeout(Duration::from_secs(5), starting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(5), async {
            while queue.test_directory_sync_attempts().await < attempts_before + 3 {
                assert!(!shutdown.is_cancelled());
                assert!(!running.worker.is_finished());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("registration must retry the prior directory-sync failure");
        let (pending, directory_sync_pending, _) = queue.test_state().await;
        assert!(directory_sync_pending);
        assert_eq!(pending.consumers.len(), 1);
        assert!(pending.primary_consumer.is_some());
        assert!(!shutdown.is_cancelled());
        assert!(!running.worker.is_finished());
        if cancel_while_pending {
            timeout(Duration::from_secs(5), running.shutdown())
                .await
                .unwrap()
                .unwrap();
            assert!(
                !shutdown.is_cancelled(),
                "requested shutdown is not a fatal worker error"
            );
            queue.test_set_persist_failure(None).await;
            return;
        }
        let barrier = Arc::new(Notify::new());
        let (peer, mut received) = peer(listener, barrier.clone());
        queue.test_set_persist_failure(None).await;
        timeout(Duration::from_secs(5), async {
            while queue.test_state().await.1 {
                assert!(!shutdown.is_cancelled());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker registration must recover after directory sync resumes");
        connection
            .deliver(&crate::harness::test_support::text(
                "registration recovered",
            ))
            .await
            .unwrap();
        assert_eq!(receive(&mut received).await["method"], "initialize");
        barrier.notify_one();
        let delivered = receive(&mut received).await;
        assert_eq!(delivered["method"], "turn/start");
        assert_eq!(delivered["params"]["threadId"], "registration-thread");
        assert!(delivered.to_string().contains("registration recovered"));
        assert!(!shutdown.is_cancelled());
        running.shutdown().await.unwrap();
        peer.abort();
    }

    #[tokio::test]
    async fn harness_session_registration_retries_uncertainty_and_delivers_after_recovery() {
        registration_uncertainty(false).await;
    }

    #[tokio::test]
    async fn harness_session_registration_retry_is_cancellable() {
        registration_uncertainty(true).await;
    }

    #[tokio::test]
    async fn harness_session_existing_public_primary_remains_a_fatal_worker_error() {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let session = CodexSession::prepare(&root, options(root.join("unused.sock")))
            .await
            .unwrap();
        let queue = session.codex_admission();
        let primary = queue
            .register_consumer("public".into(), Duration::from_secs(300), true, false)
            .await
            .unwrap();
        let connection = session.connection();
        let shutdown = CancellationToken::new();
        let running = session
            .start_with_thread_environment(
                &DeliveryConfig::default(),
                Arc::new(Guard),
                shutdown.clone(),
                || None,
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(5), shutdown.cancelled())
            .await
            .expect("an existing public primary must remain fatal to resident registration");
        let snapshot = queue.test_snapshot().await;
        assert_eq!(
            snapshot.primary_consumer.as_ref(),
            Some(&primary.consumer_id)
        );
        assert_eq!(snapshot.consumers.len(), 1);
        running.shutdown().await.unwrap();
        drop(connection);
    }

    #[tokio::test]
    async fn harness_session_without_initial_thread_waits_for_mcp_binding() {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let path = root.join("app.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let session = CodexSession::prepare(
            &root,
            CodexSessionOptions::new(
                Some(path),
                None,
                crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                    crate::core::ProviderId::new("discord").unwrap(),
                    crate::core::ModuleSlotId::new("primary").unwrap(),
                )),
            ),
        )
        .await
        .unwrap();
        let connection = session.connection();
        let running = session
            .start_with_thread_environment(
                &DeliveryConfig::default(),
                Arc::new(Guard),
                CancellationToken::new(),
                || None,
            )
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        let barrier = Arc::new(Notify::new());
        let (peer, mut received) = peer(listener, barrier.clone());
        wait_for_worker(&connection).await;
        connection
            .protocol()
            .call("bind_codex_thread", &json!({"thread_id":"late-thread"}))
            .await
            .unwrap()
            .unwrap();
        connection
            .deliver(&crate::harness::test_support::text("late binding delivery"))
            .await
            .unwrap();
        assert_eq!(receive(&mut received).await["method"], "initialize");
        barrier.notify_one();
        let delivered = receive(&mut received).await;
        assert_eq!(delivered["params"]["threadId"], "late-thread");
        assert!(delivered.to_string().contains("late binding delivery"));
        running.shutdown().await.unwrap();
        peer.abort();
    }

    #[tokio::test]
    async fn harness_session_rejects_environment_thread_before_connecting() {
        let dir = TempDir::new().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
        let path = root.join("app.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let session = CodexSession::prepare(
            &root,
            CodexSessionOptions::new(
                Some(path),
                None,
                crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                    crate::core::ProviderId::new("discord").unwrap(),
                    crate::core::ModuleSlotId::new("primary").unwrap(),
                )),
            ),
        )
        .await
        .unwrap();
        let error = match session
            .start_with_thread_environment(
                &DeliveryConfig::default(),
                Arc::new(Guard),
                CancellationToken::new(),
                || Some("\n".into()),
            )
            .await
        {
            Ok(_) => panic!("invalid environment thread must fail startup"),
            Err(error) => error,
        };
        assert_eq!(error.to_string(), "invalid CODEX_THREAD_ID");
        assert!(matches!(
            error.kind,
            HarnessSessionErrorKind::EnvironmentThread(crate::codex::CodexQueueError {
                kind: crate::codex::CodexQueueErrorKind::InvalidThreadId
            })
        ));
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interrupted_legacy_temporary_allows_real_session_initial_binding() {
        let executable = std::env::current_exe().unwrap();
        let root = TempDir::new().unwrap();
        let output = timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(executable)
                .args([
                    "--exact",
                    "harness::codex_session::tests::interrupted_legacy_temporary_initial_binding_child_022",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("DIONE_TEST_LEGACY_TEMP_ROOT", root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("owned legacy temporary child must complete")
        .unwrap();
        assert!(
            output.status.success(),
            "legacy temporary child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("DIONE_LEGACY_TEMP_CASES=3"));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "owned child invoked only by the umask-isolated parent"]
    async fn interrupted_legacy_temporary_initial_binding_child_022() {
        use std::{
            fs,
            io::Write as _,
            os::unix::fs::{DirBuilderExt as _, MetadataExt as _},
        };
        let Some(parent) = std::env::var_os("DIONE_TEST_LEGACY_TEMP_ROOT") else {
            return;
        };
        // SAFETY: this is the sole test in an explicitly spawned child process.
        unsafe { libc::umask(0o022) };
        for case in ["committed", "absent", "no-temp"] {
            let root =
                Utf8PathBuf::from_path_buf(std::path::PathBuf::from(&parent).join(case)).unwrap();
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            // SAFETY: geteuid has no arguments or memory-safety preconditions.
            let owner = unsafe { libc::geteuid() };
            let root_metadata = fs::symlink_metadata(&root).unwrap();
            assert!(root_metadata.is_dir() && !root_metadata.file_type().is_symlink());
            assert_eq!(root_metadata.uid(), owner);
            assert_eq!(root_metadata.mode() & 0o7777, 0o700);
            let snapshot = root.join("codex-inbox.json");
            let temporary = root.join("codex-inbox.json.tmp");
            let legacy_count = if case == "committed" { 2 } else { 0 };
            if case == "committed" {
                fs::write(
                    &snapshot,
                    include_bytes!("../../tests/fixtures/codex-inbox/v0.48.0-two-pending.json"),
                )
                .unwrap();
            }
            let stale_bytes = b"interrupted uncommitted bytes: not valid JSON";
            let before = if case != "no-temp" {
                // Matches the old writer's create/write/flush before its rename.
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                    .unwrap();
                file.write_all(stale_bytes).unwrap();
                file.sync_all().unwrap();
                let metadata = file.metadata().unwrap();
                assert_eq!(metadata.mode() & 0o7777, 0o644);
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(metadata.uid(), owner);
                Some((metadata.dev(), metadata.ino(), metadata.uid()))
            } else {
                None
            };
            let path = root.join("app.sock");
            let listener = UnixListener::bind(&path).unwrap();
            let session = CodexSession::prepare(&root, options(path.clone()))
                .await
                .unwrap();
            let queue = session.codex_admission();
            assert_eq!(queue.status().await.queued, legacy_count);
            if let Some(identity) = before {
                let metadata = fs::symlink_metadata(&temporary).unwrap();
                assert!(metadata.is_file() && !metadata.file_type().is_symlink());
                assert_eq!((metadata.dev(), metadata.ino(), metadata.uid()), identity);
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(metadata.mode() & 0o7777, 0o600);
                assert_eq!(fs::read(&temporary).unwrap(), stale_bytes);
            } else {
                assert!(
                    !temporary.exists(),
                    "startup must not create a missing temporary"
                );
            }
            assert_eq!(snapshot.exists(), case == "committed");
            if case == "committed" {
                assert_eq!(
                    fs::read(&snapshot).unwrap(),
                    include_bytes!("../../tests/fixtures/codex-inbox/v0.48.0-two-pending.json")
                );
            }
            let lock = root.join("codex-inbox.lock");
            let lock_before = fs::metadata(&lock).unwrap();
            assert!(matches!(
                CodexSession::prepare(&root, options(path)).await,
                Err(HarnessSessionError {
                    kind: HarnessSessionErrorKind::QueueOpen(crate::codex::CodexQueueError {
                        kind: crate::codex::CodexQueueErrorKind::Durable(
                            crate::durable::QueueError {
                                kind: crate::durable::QueueErrorKind::Locked(_),
                                ..
                            }
                        )
                    })
                })
            ));
            let binding = session.binding_rx.clone();
            let connection = session.connection();
            let barrier = Arc::new(Notify::new());
            let (peer, mut received) = peer(listener, barrier.clone());
            let running = timeout(
                Duration::from_secs(5),
                session.start_with_thread_environment(
                    &DeliveryConfig::default(),
                    Arc::new(Guard),
                    CancellationToken::new(),
                    || None,
                ),
            )
            .await
            .expect("real initial binding must complete with a legacy temporary")
            .unwrap();
            assert_eq!(
                binding.borrow().as_ref().unwrap().as_str(),
                "registration-thread"
            );
            let lock_after = fs::metadata(&lock).unwrap();
            assert_eq!(
                (lock_after.dev(), lock_after.ino()),
                (lock_before.dev(), lock_before.ino())
            );
            wait_for_worker(&connection).await;
            connection
                .deliver(&crate::harness::test_support::text(
                    "after interrupted upgrade",
                ))
                .await
                .unwrap();
            assert_eq!(receive(&mut received).await["method"], "initialize");
            barrier.notify_one();
            let delivered = receive(&mut received).await;
            assert_eq!(delivered["method"], "turn/start");
            assert_eq!(delivered["params"]["threadId"], "registration-thread");
            assert!(delivered.to_string().contains("after interrupted upgrade"));
            timeout(Duration::from_secs(5), async {
                while queue.status().await.queued != legacy_count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("new delivery must be consumed while committed backlog remains");
            running.shutdown().await.unwrap();
            peer.abort();
            let _ = peer.await;
            drop(connection);
            drop(queue);
            let reopened = CodexSession::prepare(&root, options(root.join("unused.sock")))
                .await
                .unwrap();
            assert_eq!(
                reopened.codex_admission().status().await.queued,
                legacy_count
            );
            if case == "committed" {
                let persisted: serde_json::Value =
                    serde_json::from_slice(&fs::read(snapshot).unwrap()).unwrap();
                assert_eq!(
                    persisted["entries"][0]["payload"]["params"]["content"],
                    "prior release first"
                );
                assert_eq!(
                    persisted["entries"][1]["payload"]["params"]["content"],
                    "prior release second"
                );
            }
        }
        println!("DIONE_LEGACY_TEMP_CASES=3");
    }
}
