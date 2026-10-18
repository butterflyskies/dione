use camino::Utf8PathBuf;
use dione::codex::CodexEventQueue;
use sha2::{Digest, Sha256};
use std::{
    env, fs,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CHILD_STATE_DIR: &str = "DIONE_TEST_HISTORICAL_INBOX_DIR";
const TEST_NAME: &str = "historical_v048_inbox_loads_and_preserves_entries";

#[test]
fn historical_v048_inbox_loads_and_preserves_entries() {
    if let Some(state_dir) = env::var_os(CHILD_STATE_DIR) {
        let state_dir = Utf8PathBuf::from_path_buf(state_dir.into()).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            #[cfg(unix)]
            let before = {
                use std::os::unix::fs::MetadataExt as _;
                let snapshot = state_dir.join("codex-inbox.json");
                let metadata = fs::symlink_metadata(&snapshot).unwrap();
                assert!(metadata.is_file() && !metadata.file_type().is_symlink());
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(metadata.mode() & 0o7777, 0o644);
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.uid(),
                    fs::read(snapshot).unwrap(),
                )
            };
            let queue = CodexEventQueue::load(
                &state_dir,
                dione::codex::LegacyDiscordScope::new(dione::core::ModuleInstanceId::new(
                    dione::core::ProviderId::new("discord").unwrap(),
                    dione::core::ModuleSlotId::new("primary").unwrap(),
                )),
            )
            .expect("old inbox must open");
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt as _;
                let snapshot = state_dir.join("codex-inbox.json");
                let metadata = fs::symlink_metadata(&snapshot).unwrap();
                assert!(metadata.is_file() && !metadata.file_type().is_symlink());
                assert_eq!(
                    (metadata.dev(), metadata.ino(), metadata.uid()),
                    (before.0, before.1, before.2)
                );
                assert_eq!(metadata.nlink(), 1);
                assert_eq!(metadata.mode() & 0o7777, 0o600);
                assert_eq!(fs::read(snapshot).unwrap(), before.3);
            }
            assert_eq!(queue.status().await.queued, 2);
            let consumer = queue
                .register_consumer(
                    "upgrade probe".to_owned(),
                    Duration::from_secs(60),
                    true,
                    true,
                )
                .await
                .expect("old entries must be claimable")
                .consumer_id;

            for expected in ["prior release first", "prior release second"] {
                let event = queue
                    .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
                    .await
                    .expect("old entry must be readable")
                    .expect("old entry must not disappear");
                assert_eq!(event.event["params"]["content"], expected);
                queue
                    .acknowledge(&consumer, &event.delivery_token)
                    .await
                    .expect("old entry must be acknowledgeable");
            }
            assert_eq!(queue.status().await.queued, 0);
            drop(queue);

            let reopened = CodexEventQueue::load(
                &state_dir,
                dione::codex::LegacyDiscordScope::new(dione::core::ModuleInstanceId::new(
                    dione::core::ProviderId::new("discord").unwrap(),
                    dione::core::ModuleSlotId::new("primary").unwrap(),
                )),
            )
            .expect("upgraded inbox must reopen");
            assert_eq!(reopened.status().await.queued, 0);
        });
        return;
    }

    let builder = tempfile::Builder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::PermissionsExt as _;
        let mut builder = builder;
        builder.permissions(fs::Permissions::from_mode(0o700));
        builder
    };
    let state = builder.tempdir().expect("isolated state directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let metadata = fs::symlink_metadata(state.path()).unwrap();
        assert!(metadata.is_dir() && !metadata.file_type().is_symlink());
        // SAFETY: geteuid has no memory-safety preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    }
    let fixture = fs::read("tests/fixtures/codex-inbox/v0.48.0-two-pending.json")
        .expect("historical inbox fixture must be readable");
    assert_eq!(
        format!("{:x}", Sha256::digest(&fixture)),
        "946127bd98a829fccf0bc3f3e651ee04007f376af45d30cd9397c9a57249fcfe",
        "prior-release fixture bytes changed"
    );
    let snapshot = state.path().join("codex-inbox.json");
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&snapshot)
            .expect("new synthetic historical snapshot");
        file.write_all(&fixture).unwrap();
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .unwrap();
        file.sync_all().unwrap();
        let metadata = file.metadata().unwrap();
        assert!(metadata.is_file());
        // SAFETY: geteuid has no memory-safety preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.nlink(), 1);
        assert_eq!(metadata.mode() & 0o7777, 0o644);
    }
    #[cfg(not(unix))]
    fs::write(&snapshot, fixture).expect("historical inbox fixture must be copied");
    let mut child = Command::new(env::current_exe().expect("test binary path"))
        .args(["--exact", TEST_NAME, "--nocapture"])
        .env(CHILD_STATE_DIR, state.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("upgrade probe must start");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("upgrade probe status").is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().expect("hung upgrade probe must stop");
            let _ = child.wait();
            panic!("loading and draining the v0.48.0 inbox exceeded 10 seconds");
        }
        thread::sleep(Duration::from_millis(20));
    }

    let output = child.wait_with_output().expect("upgrade probe output");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "upgrade probe matched no test:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "old inbox upgrade failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
