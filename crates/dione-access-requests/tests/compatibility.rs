use camino::Utf8PathBuf;
use chrono::Utc;
use dione_access_requests::{AccessQueue, AccessRequest};
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn state() -> (TempDir, Utf8PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
    (dir, path)
}

#[test]
fn historical_keys_and_observations_survive_write_and_reload() {
    let (_dir, path) = state();
    let historical = include_str!("fixtures/access-requests.json");
    fs::write(path.join("queue.json"), historical).unwrap();
    let queue = AccessQueue::load(&path);
    assert_eq!(
        queue.list().iter().map(|r| r.user_id).collect::<Vec<_>>(),
        vec![99, 0, u64::MAX, 42]
    );
    assert_eq!(queue.peek(7).unwrap().user_id, u64::MAX);
    assert!(queue.peek(u64::MAX).is_none());
    queue.persist().unwrap();
    let written: Value =
        serde_json::from_slice(&fs::read(path.join("queue.json")).unwrap()).unwrap();
    assert_eq!(written, serde_json::from_str::<Value>(historical).unwrap());
    let mut reloaded = AccessQueue::load(&path);
    assert_eq!(reloaded.approve(7).unwrap().user_id, u64::MAX);
    assert!(reloaded.peek(7).is_none());
    assert_eq!(reloaded.peek(0).unwrap().user_id, 0);
    assert_eq!(AccessQueue::load(&path).list().len(), 3);
}

#[test]
fn multibyte_previews_keep_the_byte_limit_and_character_boundary() {
    let (_dir, path) = state();
    let mut queue = AccessQueue::load(&path);
    for (id, text) in [(1, "é".repeat(51)), (2, format!("{}éZ", "a".repeat(99)))] {
        assert!(queue.enqueue(
            AccessRequest {
                user_id: id,
                username: "pending".to_owned(),
                message_preview: text,
                timestamp: Utc::now(),
            },
            50,
        ));
    }
    assert_eq!(queue.peek(1).unwrap().message_preview, "é".repeat(50));
    assert_eq!(queue.peek(2).unwrap().message_preview, "a".repeat(99));
    let reloaded = AccessQueue::load(&path);
    assert_eq!(reloaded.peek(1).unwrap().message_preview, "é".repeat(50));
    assert_eq!(reloaded.peek(2).unwrap().message_preview, "a".repeat(99));
}

#[test]
fn malformed_or_unreadable_state_keeps_the_empty_fallback() {
    let (_dir, path) = state();
    let queue_path = path.join("queue.json");
    assert!(AccessQueue::load(&path).list().is_empty());
    for invalid in ["{", "[]", r#"{"42":{"user_id":"bad"}}"#] {
        fs::write(&queue_path, invalid).unwrap();
        assert!(AccessQueue::load(&path).list().is_empty());
    }
    fs::remove_file(&queue_path).unwrap();
    fs::create_dir(&queue_path).unwrap();
    assert!(AccessQueue::load(&path).list().is_empty());
}
