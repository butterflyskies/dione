use camino::Utf8Path;
use dione_mutes::{GuildMute, MuteReceipt, MuteStore};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const HISTORICAL: &str = r#"{"timestamp":"2026-01-01T00:00:00Z","guild_id":0,"operation":"mute","actor":"historical","reason":null,"muted_until":"2100-01-01T00:00:00Z"}"#;

fn private_directory() -> TempDir {
    let directory = TempDir::new().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    }
    directory
}

#[tokio::test]
async fn historical_omitted_cutoff_survives_load_append_and_reload() {
    let old_mute: GuildMute = serde_json::from_str(
        r#"{"guild_id":0,"muted_until":"2100-01-01T00:00:00Z","muted_by":"historical","reason":null,"muted_at":"2026-01-01T00:00:00Z"}"#,
    ).unwrap();
    assert_eq!(old_mute.cutoff_event_id, "");
    let dir = private_directory();
    let path = Utf8Path::from_path(dir.path()).unwrap();
    let log = path.join("guild_mute_receipts.jsonl");
    fs::write(&log, format!("{HISTORICAL}\n")).unwrap();
    let store = MuteStore::load(path).await.unwrap();
    assert!(store.is_guild_muted(0));
    assert_eq!(store.load_state().mutes[&0].cutoff_event_id, "");
    assert_eq!(fs::read_to_string(&log).unwrap(), format!("{HISTORICAL}\n"));
    assert_eq!(store.unmute_guild(0).await.unwrap().muted_by, "historical");
    let contents = fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = contents.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], HISTORICAL);
    let removed: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(removed["guild_id"], 0);
    assert_eq!(removed["operation"], "unmute");
    assert_eq!(removed["actor"], "historical");
    assert!(removed.get("muted_until").is_none());
    assert!(removed.get("cutoff_event_id").is_none());
    assert!(
        MuteStore::load(path)
            .await
            .unwrap()
            .load_state()
            .mutes
            .is_empty()
    );
}

#[tokio::test]
async fn corrupt_nonblank_tail_keeps_line_error_and_original_bytes() {
    let dir = private_directory();
    let path = Utf8Path::from_path(dir.path()).unwrap();
    let log = path.join("guild_mute_receipts.jsonl");
    let corrupt = format!("{HISTORICAL}\n\n{{\n");
    fs::write(&log, &corrupt).unwrap();
    let error = MuteStore::load(path).await.err().unwrap();
    assert!(error.to_string().starts_with("corrupt receipt at line 3:"));
    assert_eq!(fs::read_to_string(&log).unwrap(), corrupt);
}

#[tokio::test]
async fn zero_ttl_remains_accepted_and_receipted_by_store() {
    let dir = private_directory();
    let path = Utf8Path::from_path(dir.path()).unwrap();
    let store = MuteStore::load(path).await.unwrap();
    let muted = store
        .mute_guild(42, 0, "recorded".to_owned(), None)
        .await
        .unwrap();
    assert_eq!(muted.guild_id, 42);
    assert!(!store.is_guild_muted(42));
    let contents = fs::read_to_string(path.join("guild_mute_receipts.jsonl")).unwrap();
    assert_eq!(contents.lines().count(), 1);
    let receipt: MuteReceipt = serde_json::from_str(contents.lines().next().unwrap()).unwrap();
    assert_eq!(receipt.guild_id, 42);
    assert_eq!(receipt.muted_until, Some(muted.muted_at));
}
