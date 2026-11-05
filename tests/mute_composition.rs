use camino::Utf8Path;
use dione::composition::mutes::{global, init_global};
use dione_mutes::{MuteState, MuteStore};
use std::{env, process::Command, sync::Arc};
use tempfile::TempDir;

#[tokio::test]
async fn composition_retains_one_shared_store_and_first_initialization() {
    const CHILD: &str = "DIONE_MUTE_COMPOSITION_CHILD";
    if env::var_os(CHILD).is_none() {
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "composition_retains_one_shared_store_and_first_initialization",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    assert!(global().is_none());
    let dir = TempDir::new().unwrap();
    let path = Utf8Path::from_path(dir.path()).unwrap();
    init_global(MuteStore::from_state(MuteState::default(), path));
    let first = global().unwrap();
    let second = global().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    first
        .mute_guild(42, 1, "recorded".to_owned(), None)
        .await
        .unwrap();
    assert!(second.is_guild_muted(42));
    init_global(MuteStore::from_state(MuteState::default(), path));
    let after_second_init = global().unwrap();
    assert!(Arc::ptr_eq(&first, &after_second_init));
    assert!(after_second_init.is_guild_muted(42));
    assert!(!after_second_init.is_guild_muted(43));
}
